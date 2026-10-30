//! [`S3TagStore`]: the client tag index on an object store, under the
//! `[storage]` bucket and prefix. The mechanics live in [`super::tag_layout`].

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use object_store::ObjectStore;

use pipette_plan_types::{ClientId, Tag};
use pipette_store::TagStore;

use super::tag_layout;

#[derive(Clone)]
pub struct S3TagStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3TagStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }
}

#[async_trait]
impl TagStore for S3TagStore {
    async fn add_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()> {
        tag_layout::add_tag(&self.store, &self.prefix, client_id, tag).await
    }

    async fn remove_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()> {
        tag_layout::remove_tag(&self.store, &self.prefix, client_id, tag).await
    }

    async fn delete_client_tags(&self, client_id: &ClientId) -> anyhow::Result<()> {
        tag_layout::delete_all_tags(&self.store, &self.prefix, client_id).await
    }

    async fn get_client_tags(&self, client_id: &ClientId) -> anyhow::Result<BTreeSet<Tag>> {
        tag_layout::list_client_tags(&self.store, &self.prefix, client_id).await
    }

    async fn list_client_ids_by_tag(&self, tag: &Tag) -> anyhow::Result<Vec<ClientId>> {
        tag_layout::list_client_ids_by_tag(&self.store, &self.prefix, tag).await
    }

    async fn list_forward_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        tag_layout::list_all_forward_markers(&self.store, &self.prefix).await
    }

    async fn list_reverse_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        tag_layout::list_all_reverse_markers(&self.store, &self.prefix).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::TryStreamExt;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    use pipette_plan_types::{ClientId, Tag};
    use pipette_store::TagStore;

    use super::S3TagStore;
    use crate::tests::write_raw_under;

    fn tag_store() -> (Arc<dyn ObjectStore>, S3TagStore) {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let tags = S3TagStore::new(store.clone(), "test-prefix".to_string());
        (store, tags)
    }

    #[tokio::test]
    async fn test_s3_tag_store_roundtrip() -> anyhow::Result<()> {
        let (store, tags) = tag_store();

        pipette_store_testkit::assert_tag_store_roundtrip(
            &tags,
            &ClientId::try_new("ev1_a")?,
            &ClientId::try_new("ev1_b")?,
            write_raw_under(&store),
        )
        .await
    }

    /// Each key is built from one format string, so a tree that moved is
    /// self-consistent and invisible to the round trip.
    #[tokio::test]
    async fn test_s3_tag_keys() -> anyhow::Result<()> {
        let (store, tags) = tag_store();

        tags.add_client_tag(&ClientId::try_new("ev1_rack")?, &Tag::try_new("lab")?)
            .await?;

        let mut keys: Vec<(String, u64)> = store
            .list(None)
            .map_ok(|meta| (meta.location.to_string(), meta.size))
            .try_collect()
            .await?;
        keys.sort();
        let empty_markers = [
            "test-prefix/tags-index/by-client/ev1_rack/lab",
            "test-prefix/tags-index/by-tag/lab/ev1_rack",
        ]
        .map(|key| (key.to_string(), 0));
        assert_eq!(
            keys, empty_markers,
            "a tag writes its two empty markers and nothing else"
        );
        Ok(())
    }
}
