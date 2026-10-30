//! [`S3CampaignStore`]: the campaign catalog on an object store. Not part of
//! the plan store: its four keyspaces are written in one pass and read together
//! by `queue-maintenance`; neither that writer nor that pass touches campaigns.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt};

use pipette_core::campaign::Campaign;
use pipette_plan_types::CampaignId;
use pipette_store::{CampaignPage, CampaignStore};

use super::{get_bytes, obj_path, page_flat_keyspace, put_bytes};

#[derive(Clone)]
pub(crate) struct S3CampaignStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3CampaignStore {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }

    fn record_path(&self, campaign_id: &CampaignId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{CAMPAIGNS_PREFIX}{}.json", campaign_id.as_str()),
        )
    }
}

const CAMPAIGNS_PREFIX: &str = "campaigns/";

#[async_trait]
impl CampaignStore for S3CampaignStore {
    async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
        match get_bytes(&self.store, &self.record_path(campaign_id)).await? {
            Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
            None => Ok(None),
        }
    }

    async fn put_campaign(&self, campaign: &Campaign) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.record_path(&campaign.campaign_id),
            serde_json::to_vec_pretty(campaign)?,
        )
        .await
    }

    async fn list_campaigns(
        &self,
        after: Option<&CampaignId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage> {
        page_flat_keyspace(
            &self.store,
            &obj_path(&self.prefix, CAMPAIGNS_PREFIX),
            |campaign_id| self.record_path(campaign_id),
            after,
            limit,
            CampaignId::try_new,
            // No filter: a campaign has no lifecycle to narrow on, and ownership
            // is a caller's scope rather than a store-side selection.
            |_: &Campaign| true,
        )
        .await
    }

    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
        match self.store.delete(&self.record_path(campaign_id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::campaign::{CampaignOwner, NewCampaign};
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, PlanId, UserId};

    use super::*;

    fn campaign_store() -> (Arc<dyn ObjectStore>, S3CampaignStore) {
        let backend: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = S3CampaignStore::new(backend.clone(), "root".to_string());
        (backend, store)
    }

    fn a_campaign() -> anyhow::Result<Campaign> {
        Ok(NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly")?,
            CampaignOwner::User {
                user_id: UserId::mint(),
            },
        )
        .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Utc::now()))
    }

    #[tokio::test]
    async fn campaign_store_meets_the_shared_contract() -> anyhow::Result<()> {
        let (_, store) = campaign_store();
        pipette_store_testkit::assert_campaign_store_roundtrip(&store).await
    }

    /// Pins the object key, for the reason the local_fs path test gives.
    #[tokio::test]
    async fn campaign_keys_are_pinned() -> anyhow::Result<()> {
        let (backend, store) = campaign_store();
        let campaign = a_campaign()?;
        store.put_campaign(&campaign).await?;
        assert!(
            backend
                .get(&ObjPath::from(format!(
                    "root/campaigns/{}.json",
                    campaign.campaign_id
                )))
                .await
                .is_ok(),
            "the record must land under campaigns/"
        );
        Ok(())
    }
}
