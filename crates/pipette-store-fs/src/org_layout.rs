//! Filesystem layout for [`super::LocalFsOrgStore`]. A record is a plain write
//! (one writer, the operator surface); only a handle claim is an exclusive
//! create.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_plan_types::{ClientId, ClientOwner, OrgId, UserId};
use pipette_store::CreateOutcome;

use crate::storage::create_complete;

/// [`Claim::Taken`] carries the holder, so the operator learns which
/// organization has the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Claim {
    Claimed,
    Taken(OrgId),
}

fn load<T: DeserializeOwned>(dir: &Path, id: &str) -> anyhow::Result<Option<T>> {
    match std::fs::read(dir.join(format!("{id}.json"))) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save<T: Serialize>(dir: &Path, id: &str, record: &T) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec_pretty(record)?,
    )?;
    Ok(())
}

fn delete(dir: &Path, id: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(format!("{id}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Every record under `dir`, in id order. One that will not parse is logged and
/// skipped, so a corrupt file does not hide the rest.
fn list<T: DeserializeOwned>(dir: &Path, what: &str) -> anyhow::Result<Vec<T>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|entry| crate::readable_entry(dir, entry))
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| match std::fs::read(&path) {
            Ok(data) => match serde_json::from_slice::<T>(&data) {
                Ok(record) => Some(Ok(record)),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, what, "skipping malformed record");
                    None
                }
            },
            // Deleted since the listing, not corrupt: skipped.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(Err(e.into())),
        })
        .collect()
}

pub(super) fn load_org(orgs_dir: &Path, org_id: &OrgId) -> anyhow::Result<Option<Organization>> {
    load(orgs_dir, org_id.as_str())
}

pub(super) fn save_org(orgs_dir: &Path, org: &Organization) -> anyhow::Result<()> {
    save(orgs_dir, org.org_id.as_str(), org)
}

pub(super) fn delete_org(orgs_dir: &Path, org_id: &OrgId) -> anyhow::Result<()> {
    delete(orgs_dir, org_id.as_str())
}

pub(super) fn list_orgs(orgs_dir: &Path) -> anyhow::Result<Vec<Organization>> {
    list(orgs_dir, "organization")
}

pub(super) fn load_membership(dir: &Path, user_id: &UserId) -> anyhow::Result<Option<OrgId>> {
    Ok(load::<Membership>(dir, user_id.as_str())?.map(|membership| membership.org_id))
}

pub(super) fn save_membership(dir: &Path, membership: &Membership) -> anyhow::Result<()> {
    save(dir, membership.user_id.as_str(), membership)
}

pub(super) fn delete_membership(dir: &Path, user_id: &UserId) -> anyhow::Result<()> {
    delete(dir, user_id.as_str())
}

pub(super) fn list_memberships(dir: &Path) -> anyhow::Result<Vec<Membership>> {
    list(dir, "membership")
}

pub(super) fn load_ownership(
    dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<Option<ClientOwner>> {
    Ok(load::<Ownership>(dir, client_id.as_str())?.map(|ownership| ownership.owner))
}

pub(super) fn save_ownership(dir: &Path, ownership: &Ownership) -> anyhow::Result<()> {
    save(dir, ownership.client_id.as_str(), ownership)
}

pub(super) fn delete_ownership(dir: &Path, client_id: &ClientId) -> anyhow::Result<()> {
    delete(dir, client_id.as_str())
}

pub(super) fn list_ownerships(dir: &Path) -> anyhow::Result<Vec<Ownership>> {
    list(dir, "ownership")
}

/// Claim `slug` for `org_id`, or report who already holds it.
///
/// Exactly one concurrent create makes the file and the rest read the winner
/// back ([`create_complete`]). The id is the body, not a path segment, so two
/// racers cannot get two keys.
///
/// `slug` is an [`OrgSlug`](pipette_plan_types::OrgSlug), `[a-z0-9_-]+`, so it
/// is one path segment and one handle is one key.
pub(super) fn claim_slug(index_dir: &Path, slug: &str, org_id: &OrgId) -> anyhow::Result<Claim> {
    let path = index_dir.join(slug);
    if create_complete(index_dir, &path, org_id.as_str().as_bytes())? == CreateOutcome::Created {
        return Ok(Claim::Claimed);
    }
    let holder = std::fs::read_to_string(&path)?;
    Ok(Claim::Taken(OrgId::try_new(holder.trim())?))
}

/// The organization holding `slug`, or `None` when the handle is unclaimed.
pub(super) fn lookup_slug(index_dir: &Path, slug: &str) -> anyhow::Result<Option<OrgId>> {
    match std::fs::read_to_string(index_dir.join(slug)) {
        Ok(holder) => Ok(Some(OrgId::try_new(holder.trim())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Release a handle claim. Idempotent.
///
/// Runs after the record is deleted: a crash between leaves a stale claim to
/// fix by hand, safer than freeing a handle that members still point at.
pub(super) fn release_slug(index_dir: &Path, slug: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(index_dir.join(slug)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
