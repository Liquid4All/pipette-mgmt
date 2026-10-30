//! [`DatasetCacheStore`] over `{prefix}/dataset-cache/artifacts/`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use tokio::io::AsyncReadExt;

use pipette_core::dataset::DatasetArtifact;
use pipette_plan_types::Sha256;
use pipette_store::{ArtifactBody, DatasetCacheStore};

use crate::{get_bytes, obj_path, put_bytes};

/// S3's floor, so an 80 MB corpus never sits in memory whole.
const PART: usize = 5 * 1024 * 1024;

pub(crate) struct S3DatasetCacheStore {
    pub(crate) store: Arc<dyn ObjectStore>,
    pub(crate) prefix: String,
}

impl S3DatasetCacheStore {
    fn tar_path(&self, digest: &Sha256) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("dataset-cache/artifacts/{digest}.tar"),
        )
    }

    fn json_path(&self, digest: &Sha256) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("dataset-cache/artifacts/{digest}.json"),
        )
    }
}

#[async_trait]
impl DatasetCacheStore for S3DatasetCacheStore {
    async fn get_artifact(&self, digest: &Sha256) -> anyhow::Result<Option<DatasetArtifact>> {
        match get_bytes(&self.store, &self.json_path(digest)).await? {
            Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
            None => Ok(None),
        }
    }

    async fn put_artifact(&self, artifact: &DatasetArtifact) -> anyhow::Result<()> {
        let body = serde_json::to_vec_pretty(artifact)?;
        put_bytes(&self.store, &self.json_path(&artifact.digest), body).await
    }

    async fn has_tar(&self, digest: &Sha256) -> anyhow::Result<bool> {
        match self.store.head(&self.tar_path(digest)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn put_tar(&self, digest: &Sha256, file: &Path) -> anyhow::Result<()> {
        // Visible only on completion: the atomicity the trait promises.
        let mut source = tokio::fs::File::open(file).await?;
        let upload = self.store.put_multipart(&self.tar_path(digest)).await?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, PART);
        let mut buf = vec![0_u8; PART];
        let streamed = async {
            loop {
                let n = source.read(&mut buf).await?;
                if n == 0 {
                    return anyhow::Ok(());
                }
                writer.wait_for_capacity(4).await?;
                let chunk = buf.get(..n).ok_or_else(|| {
                    anyhow::anyhow!("read reported {n} bytes into a {PART}-byte buffer")
                })?;
                writer.write(chunk);
            }
        }
        .await;
        // `finish` aborts only when its own `complete` fails; a part that
        // failed earlier would otherwise leave a billable incomplete upload.
        if let Err(e) = streamed {
            // `?` returns the abort's error, so the upload's is logged here.
            writer.abort().await.inspect_err(|_| {
                tracing::warn!(
                    %digest,
                    error = %format_args!("{e:#}"),
                    "failed to upload dataset tar; its abort failed as well"
                );
            })?;
            return Err(e);
        }
        writer.finish().await?;
        Ok(())
    }

    async fn open_tar(&self, digest: &Sha256) -> anyhow::Result<Option<ArtifactBody>> {
        let result = match self.store.get(&self.tar_path(digest)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = result.meta.size;
        let stream = result.into_stream().map_err(anyhow::Error::from).boxed();
        Ok(Some(ArtifactBody { bytes, stream }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dataset_cache_store_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = S3DatasetCacheStore {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: "root".to_string(),
        };
        pipette_store_testkit::assert_dataset_cache_store_roundtrip(&store, dir.path()).await
    }
}
