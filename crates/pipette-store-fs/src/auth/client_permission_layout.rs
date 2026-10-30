//! Filesystem layout for a client's permission sets: one object per client at
//! `{dir}/{client_id}.json`.
//!
//! [`super::user_permission_layout`] repeats this shape rather than sharing
//! it, so neither kind can be changed through the other.

use std::path::{Path, PathBuf};

use anyhow::Context;

use pipette_core::client_permission::ClientPermissionSet;
use pipette_plan_types::ClientId;
use pipette_store::flat_keyspace_id;

use crate::run_blocking;

/// The `{"permissions": [...]}` wrapper is a compatibility contract: a bare
/// array could not be read by a replica running an older binary.
#[derive(serde::Serialize, serde::Deserialize)]
struct PermissionsBody {
    permissions: ClientPermissionSet,
}

/// An absent object grants nothing; one holding `[]` is a set an operator
/// cleared, kept apart so a listing can tell "never granted" from "cleared".
/// A [`pipette_plan_types::ClientId`] matches `[A-Za-z0-9_-]`, so it is one path
/// segment.
pub(super) fn load(dir: &Path, client_id: &str) -> anyhow::Result<Option<ClientPermissionSet>> {
    match std::fs::read(dir.join(format!("{client_id}.json"))) {
        Ok(data) => Ok(Some(
            serde_json::from_slice::<PermissionsBody>(&data)?.permissions,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// An entry whose name is not a client id (a `.staged` leftover of [`save`]) or
/// whose body will not parse is skipped: a durable state an operator can fix,
/// read as a client holding nothing, the fail-closed direction.
///
/// A failed read fails the listing instead: the caller freezes what it computes
/// from it, so a transient fault read as an empty set would become a permanent
/// wrong answer. The name filter runs first, so a `.staged` file is never read.
pub(super) fn list_all(dir: &Path) -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let named: Vec<(ClientId, PathBuf)> = entries
        .map(|entry| entry.context("reading the permissions directory"))
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|entry| {
            let client_id = flat_keyspace_id(entry.file_name().to_str()?, ClientId::try_new)?;
            Some((client_id, entry.path()))
        })
        .collect();
    named
        .into_iter()
        .map(|(client_id, path)| {
            let data =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            Ok(serde_json::from_slice::<PermissionsBody>(&data)
                .inspect_err(|e| {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping malformed client permission set"
                    );
                })
                .ok()
                .map(|body| (client_id, body.permissions)))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|rows| rows.into_iter().flatten().collect())
}

/// Staged in full and renamed into place, so a reader never sees a torn file.
/// The rename overwrites, not create-only: a permission set changes both ways.
/// A staged leftover lacks the `.json` suffix and carries a fresh UUID, so it is
/// neither read as a set nor collides with a concurrent write.
pub(super) fn save(dir: &Path, client_id: &str, set: ClientPermissionSet) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{client_id}.json"));
    let staged = dir.join(format!("{client_id}.{}.staged", uuid::Uuid::new_v4()));
    std::fs::write(
        &staged,
        serde_json::to_vec_pretty(&PermissionsBody { permissions: set })?,
    )?;
    if let Err(e) = std::fs::rename(&staged, &path) {
        crate::remove_staged(&staged);
        return Err(e.into());
    }
    Ok(())
}

/// Delete a client's set. Idempotent.
pub(super) fn delete(dir: &Path, client_id: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(format!("{client_id}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// An accepted race: a filesystem has no conditional write, so two interleaved
/// mutations of one client lose the earlier write.
pub(super) async fn mutate(
    dir: &Path,
    client_id: &str,
    f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
) -> anyhow::Result<(bool, ClientPermissionSet)> {
    let owned_dir = dir.to_path_buf();
    let owned_id = client_id.to_string();
    let mut set = run_blocking(move || load(&owned_dir, &owned_id))
        .await?
        .unwrap_or_default();
    let changed = f(&mut set);
    if changed {
        let owned_dir = dir.to_path_buf();
        let owned_id = client_id.to_string();
        let to_write = set.clone();
        run_blocking(move || save(&owned_dir, &owned_id, to_write)).await?;
    }
    Ok((changed, set))
}
