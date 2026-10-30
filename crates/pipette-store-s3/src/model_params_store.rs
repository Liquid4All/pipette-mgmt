//! [`S3ModelParamsStore`]: model parameter records on an object store, as
//! `model-params/{key}.json` under the `[storage]` prefix.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt};

use pipette_core::model_params::ModelParams;
use pipette_plan_types::ModelKey;
use pipette_store::{ModelParamsPage, ModelParamsStore};

use super::{get_bytes, obj_path, page_flat_keyspace, put_bytes};

const MODEL_PARAMS_PREFIX: &str = "model-params/";

#[derive(Clone)]
pub(crate) struct S3ModelParamsStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3ModelParamsStore {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }

    fn record_path(&self, key: &ModelKey) -> ObjPath {
        obj_path(&self.prefix, &format!("{MODEL_PARAMS_PREFIX}{key}.json"))
    }
}

#[async_trait]
impl ModelParamsStore for S3ModelParamsStore {
    async fn get_model_params(&self, key: &ModelKey) -> anyhow::Result<Option<ModelParams>> {
        match get_bytes(&self.store, &self.record_path(key)).await? {
            Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
            None => Ok(None),
        }
    }

    async fn put_model_params(&self, params: &ModelParams) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.record_path(&params.key),
            serde_json::to_vec_pretty(params)?,
        )
        .await
    }

    async fn list_model_params(
        &self,
        after: Option<&ModelKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ModelParamsPage> {
        page_flat_keyspace(
            &self.store,
            &obj_path(&self.prefix, MODEL_PARAMS_PREFIX),
            |key| self.record_path(key),
            after,
            limit,
            ModelKey::try_new,
            |_: &ModelParams| true,
        )
        .await
    }

    async fn delete_model_params(&self, key: &ModelKey) -> anyhow::Result<()> {
        match self.store.delete(&self.record_path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use object_store::memory::InMemory;

    use super::*;

    fn model_params_store() -> (Arc<dyn ObjectStore>, S3ModelParamsStore) {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = S3ModelParamsStore::new(backend.clone(), "root".to_string());
        (backend, store)
    }

    #[tokio::test]
    async fn s3_model_params_store_meets_the_shared_contract() -> anyhow::Result<()> {
        let (_, store) = model_params_store();
        pipette_store_testkit::assert_model_params_store_roundtrip(&store).await
    }

    /// Pins the object key, which the round trip cannot see.
    #[tokio::test]
    async fn s3_model_params_key() -> anyhow::Result<()> {
        let (backend, store) = model_params_store();
        let params = pipette_store_testkit::a_model_params("acmelm-4.5b-text", 4512)?;
        store.put_model_params(&params).await?;
        backend
            .head(&ObjPath::from("root/model-params/acmelm-4.5b-text.json"))
            .await?;
        Ok(())
    }
}
