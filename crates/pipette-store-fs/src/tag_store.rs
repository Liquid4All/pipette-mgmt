//! [`LocalFsTagStore`]: the client tag index on a filesystem, under the
//! `[storage]` root. The mechanics live in [`super::tag_layout`].

use std::collections::BTreeSet;
use std::path::PathBuf;

use async_trait::async_trait;

use pipette_plan_types::{ClientId, Tag};
use pipette_store::TagStore;

use super::{run_blocking, tag_layout};

#[derive(Clone)]
pub struct LocalFsTagStore {
    by_client_dir: PathBuf,
    by_tag_dir: PathBuf,
}

impl LocalFsTagStore {
    /// `root` is the `[storage]` data directory.
    pub fn new(root: PathBuf) -> Self {
        let index = root.join("tags-index");
        Self {
            by_client_dir: index.join("by-client"),
            by_tag_dir: index.join("by-tag"),
        }
    }
}

#[async_trait]
impl TagStore for LocalFsTagStore {
    async fn add_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()> {
        let by_client_dir = self.by_client_dir.clone();
        let by_tag_dir = self.by_tag_dir.clone();
        let client_id = client_id.clone();
        let tag = tag.clone();
        run_blocking(move || tag_layout::add_tag(&by_client_dir, &by_tag_dir, &client_id, &tag))
            .await
    }

    async fn remove_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()> {
        let by_client_dir = self.by_client_dir.clone();
        let by_tag_dir = self.by_tag_dir.clone();
        let client_id = client_id.clone();
        let tag = tag.clone();
        run_blocking(move || tag_layout::remove_tag(&by_client_dir, &by_tag_dir, &client_id, &tag))
            .await
    }

    async fn delete_client_tags(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let by_client_dir = self.by_client_dir.clone();
        let by_tag_dir = self.by_tag_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || tag_layout::delete_all_tags(&by_client_dir, &by_tag_dir, &client_id))
            .await
    }

    async fn get_client_tags(&self, client_id: &ClientId) -> anyhow::Result<BTreeSet<Tag>> {
        let by_client_dir = self.by_client_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || tag_layout::list_client_tags(&by_client_dir, &client_id)).await
    }

    async fn list_client_ids_by_tag(&self, tag: &Tag) -> anyhow::Result<Vec<ClientId>> {
        let by_tag_dir = self.by_tag_dir.clone();
        let tag = tag.clone();
        run_blocking(move || tag_layout::list_client_ids_by_tag(&by_tag_dir, &tag)).await
    }

    async fn list_forward_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        let by_client_dir = self.by_client_dir.clone();
        run_blocking(move || tag_layout::list_all_forward_markers(&by_client_dir)).await
    }

    async fn list_reverse_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        let by_tag_dir = self.by_tag_dir.clone();
        run_blocking(move || tag_layout::list_all_reverse_markers(&by_tag_dir)).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pipette_plan_types::{ClientId, Tag};
    use pipette_store::TagStore;

    use super::LocalFsTagStore;
    use crate::tests::write_raw_under;

    /// Each file under `dir`: its path relative to `root`, and its size.
    fn files_under(root: &Path, dir: &Path) -> anyhow::Result<Vec<(String, u64)>> {
        std::fs::read_dir(dir)?
            .map(|entry| -> anyhow::Result<Vec<(String, u64)>> {
                let path = entry?.path();
                if path.is_dir() {
                    files_under(root, &path)
                } else {
                    let key = path.strip_prefix(root)?.to_string_lossy().into_owned();
                    Ok(vec![(key, std::fs::metadata(&path)?.len())])
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(|nested| nested.into_iter().flatten().collect())
    }

    #[tokio::test]
    async fn test_localfs_tag_store_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsTagStore::new(dir.path().to_path_buf());

        pipette_store_testkit::assert_tag_store_roundtrip(
            &store,
            &ClientId::try_new("ev1_a")?,
            &ClientId::try_new("ev1_b")?,
            write_raw_under(dir.path()),
        )
        .await
    }

    /// Each path is built from one field, so a tree that moved is
    /// self-consistent and invisible to the round trip.
    #[tokio::test]
    async fn test_localfs_tag_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsTagStore::new(dir.path().to_path_buf());

        store
            .add_client_tag(&ClientId::try_new("ev1_rack")?, &Tag::try_new("lab")?)
            .await?;

        let mut files = files_under(dir.path(), dir.path())?;
        files.sort();
        let empty_markers = [
            "tags-index/by-client/ev1_rack/lab",
            "tags-index/by-tag/lab/ev1_rack",
        ]
        .map(|path| (path.to_string(), 0));
        assert_eq!(
            files, empty_markers,
            "a tag writes its two empty markers and nothing else"
        );
        Ok(())
    }
}
