//! Filesystem layout for a user's permission sets: one object per user at
//! `{dir}/{user_id}.json`.
//!
//! [`super::client_permission_layout`] repeats this shape rather than sharing
//! it, so neither kind can be changed through the other.

use std::path::Path;

use pipette_core::user_permission::UserPermissionSet;

use crate::run_blocking;

/// The `{"permissions": [...]}` wrapper is a compatibility contract: a bare
/// array could not be read by a replica running an older binary.
#[derive(serde::Serialize, serde::Deserialize)]
struct PermissionsBody {
    permissions: UserPermissionSet,
}

/// An absent object grants nothing; one holding `[]` is a set an operator
/// cleared, kept apart so a listing can tell "never granted" from "cleared".
/// A [`pipette_plan_types::UserId`] matches `[A-Za-z0-9_-]`, so it is one path
/// segment.
pub(super) fn load(dir: &Path, user_id: &str) -> anyhow::Result<Option<UserPermissionSet>> {
    match std::fs::read(dir.join(format!("{user_id}.json"))) {
        Ok(data) => Ok(Some(
            serde_json::from_slice::<PermissionsBody>(&data)?.permissions,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Staged in full and renamed into place, so a reader never sees a torn file.
/// The rename overwrites, not create-only: a permission set changes both ways.
/// A staged leftover lacks the `.json` suffix and carries a fresh UUID, so it is
/// neither read as a set nor collides with a concurrent write.
pub(super) fn save(dir: &Path, user_id: &str, set: UserPermissionSet) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{user_id}.json"));
    let staged = dir.join(format!("{user_id}.{}.staged", uuid::Uuid::new_v4()));
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

/// Delete a user's set. Idempotent.
pub(super) fn delete(dir: &Path, user_id: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(format!("{user_id}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// An accepted race: a filesystem has no conditional write, so two interleaved
/// mutations of one user lose the earlier write.
pub(super) async fn mutate(
    dir: &Path,
    user_id: &str,
    f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
) -> anyhow::Result<(bool, UserPermissionSet)> {
    let owned_dir = dir.to_path_buf();
    let owned_id = user_id.to_string();
    let mut set = run_blocking(move || load(&owned_dir, &owned_id))
        .await?
        .unwrap_or_default();
    let changed = f(&mut set);
    if changed {
        let owned_dir = dir.to_path_buf();
        let owned_id = user_id.to_string();
        let to_write = set.clone();
        run_blocking(move || save(&owned_dir, &owned_id, to_write)).await?;
    }
    Ok((changed, set))
}
