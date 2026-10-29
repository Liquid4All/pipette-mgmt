//! Object-store layout for the pre-auth keys the two auth stores serve —
//! `{keyspace}/{key_id}.json` for the record and `{keyspace}/{key_id}.spent`
//! for the marker that retires a single-use one.
//!
//! One layout over two keyspaces, so a listing of one kind never returns the
//! other; validation is each kind's own, in `pipette_core`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutMode, PutPayload};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{delete_idempotent, get_bytes, obj_path, put_bytes};

pub(super) const CLIENT_KEYSPACE: &str = "client-preauth";

pub(super) const USER_KEYSPACE: &str = "user-preauth";

/// Every caller holds a validated [`pipette_plan_types::ClientPreauthKeyId`] or
/// [`pipette_plan_types::UserPreauthKeyId`], both of which match `[A-Za-z0-9_-]` and
/// so are one key segment with no `/` in them.
fn record_path(prefix: &str, keyspace: &str, key_id: &str) -> ObjPath {
    obj_path(prefix, &format!("{keyspace}/{key_id}.json"))
}

/// The key the spent marker lives at — a sibling of the record, so one prefix
/// listing sees both and the two are told apart by suffix.
fn marker_path(prefix: &str, keyspace: &str, key_id: &str) -> ObjPath {
    obj_path(prefix, &format!("{keyspace}/{key_id}.spent"))
}

/// Persist a freshly minted record. Unconditional: the `key_id` is 64 bits of
/// randomness minted a moment ago, so there is nothing to race with.
pub(super) async fn save<K: Serialize + Sync>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    key_id: &str,
    key: &K,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &record_path(prefix, keyspace, key_id),
        serde_json::to_vec_pretty(key)?,
    )
    .await
}

/// Absent is the answer for every way a key stops existing — spent, revoked,
/// pruned — because all three delete the record. The caller reports them
/// alike, so the route is not an enumeration oracle.
pub(super) async fn load<K: DeserializeOwned>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    key_id: &str,
) -> anyhow::Result<Option<K>> {
    match get_bytes(store, &record_path(prefix, keyspace, key_id)).await? {
        Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
        None => Ok(None),
    }
}

/// Reads only `*.json`, so the spent markers sharing the prefix are invisible
/// here.
pub(super) async fn list<K: DeserializeOwned>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
) -> anyhow::Result<Vec<K>> {
    let listing = obj_path(prefix, &format!("{keyspace}/"));
    let entries: Vec<_> = store.list(Some(&listing)).try_collect().await?;
    stream::iter(
        entries
            .into_iter()
            .filter(|meta| crate::is_json_key(meta.location.as_ref()))
            .map(|meta| async move {
                let data = get_bytes(store, &meta.location)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("pre-auth key vanished: {}", meta.location))?;
                Ok(serde_json::from_slice(&data)?)
            }),
    )
    .buffer_unordered(pipette_store::STORAGE_CONCURRENCY)
    .try_collect()
    .await
}

/// Whether this caller is the one that spent the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Spend {
    Claimed,
    AlreadySpent,
}

/// The marker, not the record's deletion, is the spend: `object_store` has no
/// conditional delete, but `If-None-Match: *` makes one concurrent consume win,
/// across replicas too. If the caller's later delete of the record never lands,
/// the marker still makes the next consume lose.
pub(super) async fn claim_spent(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    key_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Spend> {
    match store
        .put_opts(
            &marker_path(prefix, keyspace, key_id),
            PutPayload::from(now.to_rfc3339().into_bytes()),
            PutMode::Create.into(),
        )
        .await
    {
        Ok(_) => Ok(Spend::Claimed),
        Err(object_store::Error::AlreadyExists { .. }) => Ok(Spend::AlreadySpent),
        Err(e) => Err(e.into()),
    }
}

/// Delete a record. Idempotent — a missing object is a no-op, which is what
/// lets a revoke race a spend without either failing.
pub(super) async fn delete_record(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    key_id: &str,
) -> anyhow::Result<()> {
    delete_idempotent(store, &record_path(prefix, keyspace, key_id)).await
}

/// Key ids carrying a spent marker, whether or not their record survives. An
/// invalid name is skipped: this backs a prune sweep, which one name should not
/// stop.
pub(super) async fn list_spent<I>(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
) -> anyhow::Result<Vec<I>>
where
    I: TryFrom<String>,
    I::Error: std::fmt::Display,
{
    let listing = obj_path(prefix, &format!("{keyspace}/"));
    let entries: Vec<_> = store.list(Some(&listing)).try_collect().await?;
    Ok(entries
        .iter()
        .filter_map(|meta| {
            let name = meta.location.filename()?;
            I::try_from(name.strip_suffix(".spent")?.to_string())
                .inspect_err(|e| {
                    tracing::warn!(
                        key = %meta.location,
                        error = %e,
                        "skipping unparseable spent marker"
                    );
                })
                .ok()
        })
        .collect())
}

/// Sound only once the key has no record left: while a record survives, its
/// marker is the one thing keeping the key spent.
pub(super) async fn delete_spent(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    keyspace: &str,
    key_id: &str,
) -> anyhow::Result<()> {
    delete_idempotent(store, &marker_path(prefix, keyspace, key_id)).await
}
