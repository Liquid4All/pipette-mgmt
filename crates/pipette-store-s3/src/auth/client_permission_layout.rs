//! Object-store layout for a client's permission sets: one object per client
//! at `client-permissions/{client_id}.json`.
//!
//! [`super::user_permission_layout`] repeats this shape rather than sharing
//! it, so neither kind can be changed through the other.

use std::sync::Arc;

use anyhow::Context;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutPayload};

use pipette_core::client_permission::ClientPermissionSet;
use pipette_plan_types::ClientId;
use pipette_store::{STORAGE_CONCURRENCY, flat_keyspace_id};

use crate::{get_bytes, get_bytes_for_update, obj_path, put_bytes};

/// The `{"permissions": [...]}` wrapper is a compatibility contract: a bare
/// array could not be read by a replica running an older binary.
#[derive(serde::Serialize, serde::Deserialize)]
struct PermissionsBody {
    permissions: ClientPermissionSet,
}

/// Read-modify-write attempts before contention is an error: a race between two
/// operators resolves on the next attempt, and a small budget stops a stuck
/// object spinning.
const MUTATION_ATTEMPTS: usize = 3;

const KEYSPACE: &str = "client-permissions";

/// A [`pipette_plan_types::ClientId`] matches `[A-Za-z0-9_-]`, so it is one key
/// segment.
fn path(prefix: &str, client_id: &str) -> ObjPath {
    obj_path(prefix, &format!("{KEYSPACE}/{client_id}.json"))
}

/// An absent object grants nothing; one holding `[]` is a set an operator
/// cleared, kept apart so a listing can tell "never granted" from "cleared".
pub(super) async fn load(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &str,
) -> anyhow::Result<Option<ClientPermissionSet>> {
    match get_bytes(store, &path(prefix, client_id)).await? {
        Some(data) => Ok(Some(
            serde_json::from_slice::<PermissionsBody>(&data)?.permissions,
        )),
        None => Ok(None),
    }
}

/// An entry whose name is not a client id or whose body will not parse is
/// skipped: a durable state an operator can fix, read as a client holding
/// nothing, the fail-closed direction.
///
/// A failed read fails the listing instead: the caller freezes what it computes
/// from it, so a transient fault read as an empty set would become a permanent
/// wrong answer.
pub(super) async fn list_all(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>> {
    let list_prefix = obj_path(prefix, &format!("{KEYSPACE}/"));
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    let rows: Vec<Option<(ClientId, ClientPermissionSet)>> =
        stream::iter(entries.into_iter().map(|meta| async move {
            let Some(client_id) = meta
                .location
                .filename()
                .and_then(|name| flat_keyspace_id(name, ClientId::try_new))
            else {
                return anyhow::Ok(None);
            };
            let Some(data) = get_bytes(store, &meta.location)
                .await
                .with_context(|| format!("reading the permission object {}", meta.location))?
            else {
                return anyhow::Ok(None);
            };
            Ok(serde_json::from_slice::<PermissionsBody>(&data)
                .inspect_err(|e| {
                    tracing::warn!(
                        key = %meta.location,
                        error = %e,
                        "skipping malformed client permission set"
                    );
                })
                .ok()
                .map(|body| (client_id, body.permissions)))
        }))
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(rows.into_iter().flatten().collect())
}

/// The unconditional write, for paths that seed a set with nothing to race.
pub(super) async fn save(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &str,
    set: &ClientPermissionSet,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &path(prefix, client_id),
        serde_json::to_vec_pretty(&PermissionsBody {
            permissions: set.clone(),
        })?,
    )
    .await
}

/// Delete a client's set. Idempotent.
pub(super) async fn delete(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &str,
) -> anyhow::Result<()> {
    match store.delete(&path(prefix, client_id)).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read a client's set, apply `f`, and write it back under a compare-and-swap.
/// `None` reports an exhausted [`MUTATION_ATTEMPTS`] budget, with nothing
/// written. An absent object is an exclusive create, so a concurrent first write
/// is caught too.
pub(super) async fn mutate(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &str,
    f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
) -> anyhow::Result<Option<(bool, ClientPermissionSet)>> {
    let path = path(prefix, client_id);
    for _ in 0..MUTATION_ATTEMPTS {
        let read = get_bytes_for_update(store, &path).await?;
        let (mut set, mode) = match &read {
            Some((data, version)) => (
                serde_json::from_slice::<PermissionsBody>(data)?.permissions,
                PutMode::Update(version.clone()),
            ),
            None => (ClientPermissionSet::default(), PutMode::Create),
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
                    %client_id,
                    "client permission set changed by a concurrent write"
                );
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

pub(super) fn contention_error(client_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "permissions for {client_id} changed under {MUTATION_ATTEMPTS} \
         successive read-modify-write attempts; nothing was written"
    )
}
