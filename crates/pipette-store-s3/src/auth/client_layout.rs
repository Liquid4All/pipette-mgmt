//! Object-store layout for the client records and signature migration markers
//! that [`super::S3ClientStore`] serves. Permission sets are
//! [`super::client_permission_layout`]'s.

use std::num::NonZeroUsize;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutPayload};

use pipette_core::client::{
    Client, MigrationRecord, SignatureMigration, parse_client_or_self_heal,
};
use pipette_core::types::DeriveFromKey;
use pipette_plan_types::{ClientId, PublicKeyHex};
use pipette_store::{ClientPage, STORAGE_CONCURRENCY, UnreadableObjectError, flat_keyspace_id};

use crate::{delete_idempotent, get_bytes, get_bytes_for_update, obj_path, put_bytes};

fn client_path(prefix: &str, client_id: &ClientId) -> ObjPath {
    obj_path(prefix, &format!("clients/{client_id}.json"))
}

fn signature_migration_path(prefix: &str, client_id: &ClientId) -> ObjPath {
    obj_path(prefix, &format!("signature-migration/{client_id}.json"))
}

pub(super) async fn load_client(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<Option<Client>> {
    let path = client_path(prefix, client_id);
    match get_bytes_for_update(store, &path).await? {
        Some((data, version)) => {
            let (client, repaired) = parse_client_or_self_heal(&data)?;
            if repaired {
                persist_self_healed_client(store, &path, &client, version).await;
            }
            Ok(Some(client))
        }
        None => Ok(None),
    }
}

pub(super) async fn save_client(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client: &Client,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &client_path(prefix, &client.client_id),
        serde_json::to_vec_pretty(client)?,
    )
    .await
}

pub(super) async fn replace_client(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    read: &Client,
    updated: &Client,
) -> anyhow::Result<bool> {
    let path = client_path(prefix, &read.client_id);
    let Some((data, version)) = get_bytes_for_update(store, &path).await? else {
        return Ok(false);
    };
    let (current, _repaired) = parse_client_or_self_heal(&data)?;
    if current != *read {
        return Ok(false);
    }
    match store
        .put_opts(
            &path,
            PutPayload::from(serde_json::to_vec_pretty(updated)?),
            PutMode::Update(version).into(),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(object_store::Error::Precondition { .. } | object_store::Error::NotFound { .. }) => {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Only the record: the permission set keyed by the same id is the caller's to
/// drop.
pub(super) async fn delete_client(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    delete_idempotent(store, &client_path(prefix, client_id)).await
}

/// Every client id under `clients/`, ascending, from the listing alone: a paged
/// caller reads only the page's records. A key that is not `{client_id}.json`
/// is skipped, so foreign cruft cannot take the fleet listing down.
pub(super) async fn list_ids(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<ClientId>> {
    let list_prefix = obj_path(prefix, "clients/");
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    let mut ids: Vec<ClientId> = entries
        .iter()
        .filter_map(|meta| {
            meta.location
                .filename()
                .and_then(|name| flat_keyspace_id(name, ClientId::try_new))
        })
        .collect();
    // S3 lists lexicographically already; sorting makes the order this method's
    // promise rather than a backend's.
    ids.sort();
    Ok(ids)
}

/// One page of `clients/`, in ascending key order, per the
/// `ClientStore::list_clients` contract.
pub(super) async fn list_page(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    after: Option<&ClientId>,
    limit: NonZeroUsize,
) -> anyhow::Result<ClientPage> {
    let list_prefix = obj_path(prefix, "clients/");
    // Not a client-side skip: `AmazonS3` routes the offset to S3's `start-after`,
    // so a later page lists only the tail.
    let entries: Vec<_> = match after {
        Some(after) => {
            store
                .list_with_offset(Some(&list_prefix), &client_path(prefix, after))
                .try_collect()
                .await?
        }
        None => store.list(Some(&list_prefix)).try_collect().await?,
    };
    // A leaf that names no client is not an object of this keyspace, so it is
    // neither a row nor an unreadable entry.
    let mut candidates: Vec<(ObjPath, ClientId)> = entries
        .into_iter()
        .filter_map(|meta| {
            let leaf = meta.location.filename()?;
            let client_id = flat_keyspace_id(leaf, ClientId::try_new)?;
            (meta.location == client_path(prefix, &client_id)).then_some((meta.location, client_id))
        })
        .collect();
    let total = candidates.len();
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));

    // `buffered`, not `buffer_unordered`: the page is consumed in key order and
    // stops at `limit`, so unconsumed reads go unreported and the next page
    // decides them, once.
    let mut reads = stream::iter(
        candidates
            .into_iter()
            .map(|(location, client_id)| async move {
                // Removed since the listing: as if listed a moment later.
                let Some((data, version)) = get_bytes_for_update(store, &location).await? else {
                    return anyhow::Ok((client_id, None));
                };
                let outcome = match parse_client_or_self_heal(&data) {
                    Ok((client, repaired)) => {
                        // Idempotent; paging retries a repair at most once per page.
                        if repaired {
                            persist_self_healed_client(store, &location, &client, version).await;
                        }
                        Ok(client)
                    }
                    Err(e) => Err(UnreadableObjectError {
                        key: location.to_string(),
                        reason: e.to_string(),
                    }),
                };
                anyhow::Ok((client_id, Some(outcome)))
            }),
    )
    .buffered(STORAGE_CONCURRENCY);

    let mut page = ClientPage::default();
    let mut examined = 0usize;
    let mut last: Option<ClientId> = None;
    while let Some(read) = reads.next().await {
        let (client_id, outcome) = read?;
        examined += 1;
        last = Some(client_id);
        match outcome {
            Some(Ok(client)) => page.items.push(client),
            Some(Err(bad)) => page.unreadable.push(bad),
            None => {}
        }
        if page.items.len() == limit.get() {
            break;
        }
    }
    // `None` once the candidates ran out, so a full last page still ends a drain.
    page.resume_after = (examined < total).then_some(last).flatten();
    Ok(page)
}

pub(super) async fn find_by_public_key(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    public_key: &PublicKeyHex,
) -> anyhow::Result<bool> {
    let client_id = ClientId::derive(public_key);
    match store.head(&client_path(prefix, &client_id)).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Best-effort, conditioned on the version read: the caller already holds the
/// repaired client, so its read must not fail over a write that only helps the
/// next reader.
async fn persist_self_healed_client(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
    client: &Client,
    version: object_store::UpdateVersion,
) {
    let data = match serde_json::to_vec_pretty(client) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(
                path = %path,
                client_id = %client.client_id,
                error = %e,
                "failed to serialize self-healed client record"
            );
            return;
        }
    };

    match store
        .put_opts(
            path,
            PutPayload::from(data),
            PutMode::Update(version).into(),
        )
        .await
    {
        Ok(_) => tracing::warn!(
            path = %path,
            client_id = %client.client_id,
            "self-healed malformed client record"
        ),
        Err(object_store::Error::Precondition { .. }) => tracing::warn!(
            path = %path,
            client_id = %client.client_id,
            "skipped self-heal because client record changed during repair"
        ),
        Err(e) => tracing::warn!(
            path = %path,
            client_id = %client.client_id,
            error = %e,
            "failed to persist self-healed client record"
        ),
    }
}

pub(super) async fn has_signature_migration(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<bool> {
    match store
        .head(&signature_migration_path(prefix, client_id))
        .await
    {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(super) async fn record_signature_migration(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
    at: DateTime<Utc>,
) -> anyhow::Result<MigrationRecord> {
    let path = signature_migration_path(prefix, client_id);
    let record = SignatureMigration { first_seen: at };
    match store
        .put_opts(
            &path,
            PutPayload::from(serde_json::to_vec_pretty(&record)?),
            PutMode::Create.into(),
        )
        .await
    {
        Ok(_) => Ok(MigrationRecord::First),
        // The conditional create keeps `first_seen` the first sighting.
        Err(object_store::Error::AlreadyExists { .. }) => Ok(MigrationRecord::Existing),
        Err(e) => Err(e.into()),
    }
}

/// Skips unreadable entries per the `ClientStore::list_signature_migrations`
/// contract.
pub(super) async fn list_signature_migrations(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<(ClientId, SignatureMigration)>> {
    let list_prefix = obj_path(prefix, "signature-migration/");
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    Ok(stream::iter(entries.into_iter().map(|meta| async move {
        let name = meta.location.filename()?;
        let client_id = flat_keyspace_id(name, ClientId::try_new)?;
        let data = get_bytes(store, &meta.location)
            .await
            .inspect_err(|e| {
                tracing::warn!(
                    key = %meta.location,
                    error = %format_args!("{e:#}"),
                    "skipping unreadable signature-migration marker"
                );
            })
            .ok()??;
        let migration = serde_json::from_slice::<SignatureMigration>(&data)
            .inspect_err(|e| {
                tracing::warn!(
                    key = %meta.location,
                    error = %e,
                    "skipping malformed signature-migration marker"
                );
            })
            .ok()?;
        Some((client_id, migration))
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .filter_map(|entry| async move { entry })
    .collect::<Vec<_>>()
    .await)
}
