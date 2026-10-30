//! Object-store layout for [`super::S3OrgStore`]. A record is a plain put (one
//! writer, the operator surface); only a handle claim is a conditional create.

use std::sync::Arc;

use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjPath;
use serde::Serialize;
use serde::de::DeserializeOwned;

use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_plan_types::{ClientId, ClientOwner, OrgId, UserId};
use pipette_store::CreateOutcome;

use crate::{delete_idempotent, get_bytes, obj_path, put_bytes, put_create};

/// Where the organization records live.
pub(super) const ORGS_KEYSPACE: &str = "orgs";

/// Where the handle claims live.
pub(super) const SLUG_INDEX_KEYSPACE: &str = "org-slug-index";

/// Where the memberships live.
pub(super) const USER_ORGS_KEYSPACE: &str = "user-orgs";

/// Where the ownerships live.
pub(super) const CLIENT_OWNERS_KEYSPACE: &str = "client-owners";

/// [`Claim::Taken`] carries the holder, so the operator learns which
/// organization has the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Claim {
    Claimed,
    Taken(OrgId),
}

fn record_path(prefix: &str, keyspace: &str, id: &str) -> ObjPath {
    obj_path(prefix, &format!("{keyspace}/{id}.json"))
}

/// `slug` is an [`OrgSlug`](pipette_plan_types::OrgSlug), `[a-z0-9_-]+`, so it
/// is one key segment and one handle is one key.
fn slug_path(prefix: &str, slug: &str) -> ObjPath {
    obj_path(prefix, &format!("{SLUG_INDEX_KEYSPACE}/{slug}"))
}

async fn load<T: DeserializeOwned>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    id: &str,
) -> anyhow::Result<Option<T>> {
    match get_bytes(store, &record_path(prefix, keyspace, id)).await? {
        Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
        None => Ok(None),
    }
}

async fn save<T: Serialize + Sync>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    id: &str,
    record: &T,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &record_path(prefix, keyspace, id),
        serde_json::to_vec_pretty(record)?,
    )
    .await
}

/// Every record under `keyspace`, in id order. One that will not parse is logged
/// and skipped, so a corrupt object does not hide the rest.
async fn list<T: DeserializeOwned + Send>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
) -> anyhow::Result<Vec<T>> {
    let listing = obj_path(prefix, &format!("{keyspace}/"));
    let entries: Vec<_> = store.list(Some(&listing)).try_collect().await?;
    // Owned keys, so each read borrows nothing from `entries`: a borrowing
    // closure here cannot satisfy the higher-ranked bound `buffered` needs.
    let mut keys: Vec<ObjPath> = entries
        .into_iter()
        .map(|meta| meta.location)
        .filter(|key| key.filename().is_some_and(crate::is_json_key))
        .collect();
    keys.sort();
    let records = keys.into_iter().map(|key| async move {
        // Deleted since the listing, not corrupt: skipped.
        let Some(data) = get_bytes(store, &key).await? else {
            return Ok::<Option<T>, anyhow::Error>(None);
        };
        match serde_json::from_slice(&data) {
            Ok(record) => Ok(Some(record)),
            Err(e) => {
                tracing::warn!(key = %key, error = %e, keyspace, "skipping malformed record");
                Ok(None)
            }
        }
    });
    // `buffered` keeps the key order the listing was sorted into.
    Ok(stream::iter(records)
        .buffered(pipette_store::STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect())
}

pub(super) async fn load_org(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    org_id: &OrgId,
) -> anyhow::Result<Option<Organization>> {
    load(store, prefix, ORGS_KEYSPACE, org_id.as_str()).await
}

pub(super) async fn save_org(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    org: &Organization,
) -> anyhow::Result<()> {
    save(store, prefix, ORGS_KEYSPACE, org.org_id.as_str(), org).await
}

pub(super) async fn delete_org(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    org_id: &OrgId,
) -> anyhow::Result<()> {
    delete_idempotent(store, &record_path(prefix, ORGS_KEYSPACE, org_id.as_str())).await
}

pub(super) async fn list_orgs(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<Organization>> {
    list(store, prefix, ORGS_KEYSPACE).await
}

pub(super) async fn load_membership(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<Option<OrgId>> {
    let membership: Option<Membership> =
        load(store, prefix, USER_ORGS_KEYSPACE, user_id.as_str()).await?;
    Ok(membership.map(|membership| membership.org_id))
}

pub(super) async fn save_membership(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    membership: &Membership,
) -> anyhow::Result<()> {
    let id = membership.user_id.as_str();
    save(store, prefix, USER_ORGS_KEYSPACE, id, membership).await
}

pub(super) async fn delete_membership(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<()> {
    let path = record_path(prefix, USER_ORGS_KEYSPACE, user_id.as_str());
    delete_idempotent(store, &path).await
}

pub(super) async fn list_memberships(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<Membership>> {
    list(store, prefix, USER_ORGS_KEYSPACE).await
}

pub(super) async fn load_ownership(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<Option<ClientOwner>> {
    let ownership: Option<Ownership> =
        load(store, prefix, CLIENT_OWNERS_KEYSPACE, client_id.as_str()).await?;
    Ok(ownership.map(|ownership| ownership.owner))
}

pub(super) async fn save_ownership(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    ownership: &Ownership,
) -> anyhow::Result<()> {
    let id = ownership.client_id.as_str();
    save(store, prefix, CLIENT_OWNERS_KEYSPACE, id, ownership).await
}

pub(super) async fn delete_ownership(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    let path = record_path(prefix, CLIENT_OWNERS_KEYSPACE, client_id.as_str());
    delete_idempotent(store, &path).await
}

pub(super) async fn list_ownerships(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<Ownership>> {
    list(store, prefix, CLIENT_OWNERS_KEYSPACE).await
}

/// Claim `slug` for `org_id`, or report who already holds it.
///
/// [`put_create`] sends `If-None-Match: *`, so exactly one concurrent create
/// wins, across replicas, and the rest read the winner back. The id is the
/// body, not a key segment, so two racers cannot get two keys.
pub(super) async fn claim_slug(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    slug: &str,
    org_id: &OrgId,
) -> anyhow::Result<Claim> {
    let path = slug_path(prefix, slug);
    match put_create(store, &path, org_id.as_str().as_bytes().to_vec()).await? {
        CreateOutcome::Created => Ok(Claim::Claimed),
        CreateOutcome::AlreadyExists => {
            let holder = get_bytes(store, &path)
                .await?
                .ok_or_else(|| anyhow::anyhow!("handle claim for {slug} vanished mid-create"))?;
            Ok(Claim::Taken(OrgId::try_new(
                String::from_utf8(holder.to_vec())?.trim(),
            )?))
        }
    }
}

/// The organization holding `slug`, or `None` when the handle is unclaimed.
pub(super) async fn lookup_slug(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    slug: &str,
) -> anyhow::Result<Option<OrgId>> {
    match get_bytes(store, &slug_path(prefix, slug)).await? {
        Some(holder) => Ok(Some(OrgId::try_new(
            String::from_utf8(holder.to_vec())?.trim(),
        )?)),
        None => Ok(None),
    }
}

/// Release a handle claim. Idempotent.
///
/// Runs after the record is deleted: a crash between leaves a stale claim to
/// fix by hand, safer than freeing a handle that members still point at.
pub(super) async fn release_slug(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    slug: &str,
) -> anyhow::Result<()> {
    delete_idempotent(store, &slug_path(prefix, slug)).await
}
