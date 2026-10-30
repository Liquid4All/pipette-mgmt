//! Object-store layout for a user's permission sets: one object per user
//! at `user-permissions/{user_id}.json`.
//!
//! [`super::client_permission_layout`] repeats this shape rather than sharing
//! it, so neither kind can be changed through the other.

use std::sync::Arc;

use object_store::ObjectStoreExt;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutMode, PutPayload};

use pipette_core::user_permission::UserPermissionSet;

use crate::{get_bytes, get_bytes_for_update, obj_path, put_bytes};

/// The `{"permissions": [...]}` wrapper is a compatibility contract: a bare
/// array could not be read by a replica running an older binary.
#[derive(serde::Serialize, serde::Deserialize)]
struct PermissionsBody {
    permissions: UserPermissionSet,
}

/// Read-modify-write attempts before contention is an error: a race between two
/// operators resolves on the next attempt, and a small budget stops a stuck
/// object spinning.
const MUTATION_ATTEMPTS: usize = 3;

const KEYSPACE: &str = "user-permissions";

/// A [`pipette_plan_types::UserId`] matches `[A-Za-z0-9_-]`, so it is one key
/// segment.
fn path(prefix: &str, user_id: &str) -> ObjPath {
    obj_path(prefix, &format!("{KEYSPACE}/{user_id}.json"))
}

/// An absent object grants nothing; one holding `[]` is a set an operator
/// cleared, kept apart so a listing can tell "never granted" from "cleared".
pub(super) async fn load(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &str,
) -> anyhow::Result<Option<UserPermissionSet>> {
    match get_bytes(store, &path(prefix, user_id)).await? {
        Some(data) => Ok(Some(
            serde_json::from_slice::<PermissionsBody>(&data)?.permissions,
        )),
        None => Ok(None),
    }
}

/// The unconditional write, for paths that seed a set with nothing to race.
pub(super) async fn save(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &str,
    set: &UserPermissionSet,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &path(prefix, user_id),
        serde_json::to_vec_pretty(&PermissionsBody {
            permissions: set.clone(),
        })?,
    )
    .await
}

/// Delete a user's set. Idempotent.
pub(super) async fn delete(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &str,
) -> anyhow::Result<()> {
    match store.delete(&path(prefix, user_id)).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read a user's set, apply `f`, and write it back under a compare-and-swap.
/// `None` reports an exhausted [`MUTATION_ATTEMPTS`] budget, with nothing
/// written. An absent object is an exclusive create, so a concurrent first write
/// is caught too.
pub(super) async fn mutate(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &str,
    f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
) -> anyhow::Result<Option<(bool, UserPermissionSet)>> {
    let path = path(prefix, user_id);
    for _ in 0..MUTATION_ATTEMPTS {
        let read = get_bytes_for_update(store, &path).await?;
        let (mut set, mode) = match &read {
            Some((data, version)) => (
                serde_json::from_slice::<PermissionsBody>(data)?.permissions,
                PutMode::Update(version.clone()),
            ),
            None => (UserPermissionSet::default(), PutMode::Create),
        };
        let changed = f(&mut set);
        if !changed {
            return Ok(Some((changed, set)));
        }
        match store
            .put_opts(
                &path,
                PutPayload::from(serde_json::to_vec_pretty(&PermissionsBody {
                    permissions: set.clone(),
                })?),
                mode.into(),
            )
            .await
        {
            Ok(_) => return Ok(Some((changed, set))),
            // A concurrent write landed: read again and replay `f` against it.
            Err(object_store::Error::Precondition { .. })
            | Err(object_store::Error::AlreadyExists { .. }) => {
                tracing::debug!(
                    %user_id,
                    "user permission set changed by a concurrent write"
                );
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

pub(super) fn contention_error(user_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "permissions for {user_id} changed under {MUTATION_ATTEMPTS} \
         successive read-modify-write attempts; nothing was written"
    )
}
