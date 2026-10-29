//! [`LocalFsModelParamsStore`]: model parameter records on a filesystem, as
//! `{data_dir}/model-params/{key}.json`.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use async_trait::async_trait;

use pipette_core::model_params::ModelParams;
use pipette_core::tmp_file::TmpFileGuard;
use pipette_plan_types::ModelKey;
use pipette_store::{ModelParamsPage, ModelParamsStore};

use super::{page_flat_keyspace, remove_file_idempotent, run_blocking};

#[derive(Clone)]
pub(crate) struct LocalFsModelParamsStore {
    dir: PathBuf,
}

impl LocalFsModelParamsStore {
    pub(crate) fn new(data_dir: &std::path::Path) -> Self {
        Self {
            dir: data_dir.join("model-params"),
        }
    }

    fn record_path(&self, key: &ModelKey) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

#[async_trait]
impl ModelParamsStore for LocalFsModelParamsStore {
    async fn get_model_params(&self, key: &ModelKey) -> anyhow::Result<Option<ModelParams>> {
        let path = self.record_path(key);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn put_model_params(&self, params: &ModelParams) -> anyhow::Result<()> {
        let dir = self.dir.clone();
        let path = self.record_path(&params.key);
        let bytes = serde_json::to_vec_pretty(params)?;
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Atomic replace, as `put_campaign`; `.tmp-` has no `.json`, so a
            // listing skips it.
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let guard = TmpFileGuard::new(tmp.clone());
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
            guard.disarm();
            Ok(())
        })
        .await
    }

    async fn list_model_params(
        &self,
        after: Option<&ModelKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ModelParamsPage> {
        let dir = self.dir.clone();
        let after = after.map(|key| key.as_str().to_string());
        run_blocking(move || {
            page_flat_keyspace(&dir, after.as_deref(), limit, ModelKey::try_new, |_| true)
        })
        .await
    }

    async fn delete_model_params(&self, key: &ModelKey) -> anyhow::Result<()> {
        remove_file_idempotent(self.record_path(key)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_localfs_model_params_store_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_model_params_store_roundtrip(&LocalFsModelParamsStore::new(
            dir.path(),
        ))
        .await
    }

    /// Pins the on-disk layout, which the round trip cannot see.
    #[tokio::test]
    async fn test_localfs_model_params_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsModelParamsStore::new(dir.path());
        let params = pipette_store_testkit::a_model_params("acmelm-4.5b-text", 4512)?;
        store.put_model_params(&params).await?;
        assert!(
            dir.path()
                .join("model-params/acmelm-4.5b-text.json")
                .exists()
        );
        Ok(())
    }
}
