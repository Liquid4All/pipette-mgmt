//! [`DatasetCacheStore`] over `{data_dir}/dataset-cache/artifacts/`, and
//! [`EvalPackageStore`] over `{data_dir}/scores-packages/tars/`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::AsyncReadExt;

use pipette_core::dataset::DatasetArtifact;
use pipette_plan_types::Sha256;
use pipette_store::{ArtifactBody, DatasetCacheStore, EvalPackageStore};

use crate::run_blocking;
use crate::storage::replace_atomically;

/// An 80 MB corpus is a few hundred reads, little of it in memory at once.
const CHUNK: usize = 256 * 1024;

pub(crate) struct LocalFsDatasetCacheStore {
    base_dir: PathBuf,
}

impl LocalFsDatasetCacheStore {
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn tar_path(&self, digest: &Sha256) -> PathBuf {
        self.base_dir.join(format!("{digest}.tar"))
    }

    fn json_path(&self, digest: &Sha256) -> PathBuf {
        self.base_dir.join(format!("{digest}.json"))
    }
}

#[async_trait]
impl DatasetCacheStore for LocalFsDatasetCacheStore {
    async fn get_artifact(&self, digest: &Sha256) -> anyhow::Result<Option<DatasetArtifact>> {
        let path = self.json_path(digest);
        run_blocking(move || match std::fs::read(&path) {
            Ok(body) => Ok(Some(serde_json::from_slice(&body)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn put_artifact(&self, artifact: &DatasetArtifact) -> anyhow::Result<()> {
        let body = serde_json::to_vec_pretty(artifact)?;
        let base_dir = self.base_dir.clone();
        let dest = self.json_path(&artifact.digest);
        run_blocking(move || replace_atomically(&base_dir, &dest, |tmp| std::fs::write(tmp, body)))
            .await
    }

    async fn has_tar(&self, digest: &Sha256) -> anyhow::Result<bool> {
        Ok(tokio::fs::try_exists(self.tar_path(digest)).await?)
    }

    async fn put_tar(&self, digest: &Sha256, file: &Path) -> anyhow::Result<()> {
        let base_dir = self.base_dir.clone();
        let dest = self.tar_path(digest);
        let source = file.to_path_buf();
        run_blocking(move || {
            replace_atomically(&base_dir, &dest, |tmp| {
                std::fs::copy(&source, tmp).map(drop)
            })
        })
        .await
    }

    async fn open_tar(&self, digest: &Sha256) -> anyhow::Result<Option<ArtifactBody>> {
        open(&self.tar_path(digest)).await
    }
}

async fn open(path: &Path) -> anyhow::Result<Option<ArtifactBody>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let bytes = file.metadata().await?.len();
    let stream = futures::stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0_u8; CHUNK];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some((bytes::Bytes::from(buf), file)))
    })
    .boxed();
    Ok(Some(ArtifactBody { bytes, stream }))
}

pub(crate) struct LocalFsEvalPackageStore {
    base_dir: PathBuf,
}

impl LocalFsEvalPackageStore {
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn tar_path(&self, digest: &Sha256) -> PathBuf {
        self.base_dir.join(format!("{digest}.tar"))
    }
}

#[async_trait]
impl EvalPackageStore for LocalFsEvalPackageStore {
    async fn package_bytes(&self, digest: &Sha256) -> anyhow::Result<Option<u64>> {
        match tokio::fs::metadata(self.tar_path(digest)).await {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn open_package(&self, digest: &Sha256) -> anyhow::Result<Option<ArtifactBody>> {
        open(&self.tar_path(digest)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dataset_cache_store_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsDatasetCacheStore::new(dir.path().join("dataset-cache/artifacts"));
        pipette_store_testkit::assert_dataset_cache_store_roundtrip(&store, dir.path()).await
    }

    #[tokio::test]
    async fn eval_package_store_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsEvalPackageStore::new(dir.path().join("scores-packages/tars"));
        pipette_store_testkit::assert_eval_package_store_contract(&store, |key, body| {
            let path = dir.path().join(key);
            async move {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&path, body).await?;
                Ok(())
            }
        })
        .await
    }
}
