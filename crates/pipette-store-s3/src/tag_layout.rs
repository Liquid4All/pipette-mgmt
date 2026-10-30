//! Object-store layout for [`super::S3TagStore`]: an empty marker per
//! `(client, tag)` at `tags-index/by-client/{client_id}/{tag}` and at
//! `tags-index/by-tag/{tag}/{client_id}`. Tags and ids hold no `/`.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjPath;

use pipette_core::types::id_from_key;
use pipette_plan_types::{ClientId, Tag};
use pipette_store::STORAGE_CONCURRENCY;

use crate::{delete_idempotent, obj_path, put_bytes};

fn forward_tag_path(prefix: &str, client_id: &ClientId, tag: &Tag) -> ObjPath {
    obj_path(prefix, &format!("tags-index/by-client/{client_id}/{tag}"))
}

fn reverse_tag_path(prefix: &str, tag: &Tag, client_id: &ClientId) -> ObjPath {
    obj_path(prefix, &format!("tags-index/by-tag/{tag}/{client_id}"))
}

/// Which of a tag-marker key's two trailing segments is which.
enum MarkerOrder {
    ClientThenTag,
    TagThenClient,
}

fn parse_marker_pair(key: &str, order: MarkerOrder) -> Option<(ClientId, Tag)> {
    let mut segs = key.rsplit('/');
    let (last, prev) = (segs.next()?, segs.next()?);
    let (id, tag) = match order {
        MarkerOrder::ClientThenTag => (prev, last),
        MarkerOrder::TagThenClient => (last, prev),
    };
    id_from_key(id, ClientId::try_new)
        .map_err(anyhow::Error::from)
        .and_then(|client_id| Ok((client_id, Tag::try_new(tag)?)))
        .inspect_err(|e| {
            tracing::warn!(%key, error = %e, "skipping unparseable tag-index entry");
        })
        .ok()
}

/// Add a `(client, tag)` membership — writes both markers; idempotent.
pub(super) async fn add_tag(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
    tag: &Tag,
) -> anyhow::Result<()> {
    put_bytes(store, &forward_tag_path(prefix, client_id, tag), Vec::new()).await?;
    put_bytes(store, &reverse_tag_path(prefix, tag, client_id), Vec::new()).await
}

/// Remove a `(client, tag)` membership from both trees. Idempotent.
pub(super) async fn remove_tag(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
    tag: &Tag,
) -> anyhow::Result<()> {
    delete_idempotent(store, &forward_tag_path(prefix, client_id, tag)).await?;
    delete_idempotent(store, &reverse_tag_path(prefix, tag, client_id)).await
}

pub(super) async fn list_client_tags(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<BTreeSet<Tag>> {
    let list_prefix = obj_path(prefix, &format!("tags-index/by-client/{client_id}/"));
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    Ok(entries
        .iter()
        .filter_map(|meta| {
            let name = meta.location.as_ref().rsplit('/').next()?;
            Tag::try_new(name)
                .inspect_err(|e| {
                    tracing::warn!(
                        key = %meta.location,
                        error = %e,
                        "skipping unparseable tag-index entry"
                    );
                })
                .ok()
        })
        .collect())
}

pub(super) async fn list_client_ids_by_tag(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    tag: &Tag,
) -> anyhow::Result<Vec<ClientId>> {
    let list_prefix = obj_path(prefix, &format!("tags-index/by-tag/{tag}/"));
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    let mut ids: Vec<ClientId> = entries
        .iter()
        .filter_map(|meta| {
            let name = meta.location.as_ref().rsplit('/').next()?;
            id_from_key(name, ClientId::try_new)
                .inspect_err(|e| {
                    tracing::warn!(
                        key = %meta.location,
                        error = %e,
                        "skipping unparseable tag-index entry"
                    );
                })
                .ok()
        })
        .collect();
    ids.sort();
    Ok(ids)
}

/// Every `(client_id, tag)` in the forward tree, for the reverse-index
/// reconcile.
pub(super) async fn list_all_forward_markers(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<(ClientId, Tag)>> {
    let list_prefix = obj_path(prefix, "tags-index/by-client/");
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    Ok(entries
        .iter()
        .filter_map(|meta| parse_marker_pair(meta.location.as_ref(), MarkerOrder::ClientThenTag))
        .collect())
}

/// Every `(client_id, tag)` in the reverse tree, for the reconcile.
pub(super) async fn list_all_reverse_markers(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<(ClientId, Tag)>> {
    let list_prefix = obj_path(prefix, "tags-index/by-tag/");
    let entries: Vec<_> = store.list(Some(&list_prefix)).try_collect().await?;
    Ok(entries
        .iter()
        .filter_map(|meta| parse_marker_pair(meta.location.as_ref(), MarkerOrder::TagThenClient))
        .collect())
}

/// Remove every tag membership for a client (used on delete).
pub(super) async fn delete_all_tags(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    let paths: Vec<ObjPath> = list_client_tags(store, prefix, client_id)
        .await?
        .iter()
        .flat_map(|tag| {
            [
                forward_tag_path(prefix, client_id, tag),
                reverse_tag_path(prefix, tag, client_id),
            ]
        })
        .collect();
    stream::iter(
        paths
            .into_iter()
            .map(|p| async move { delete_idempotent(store, &p).await }),
    )
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;
    Ok(())
}
