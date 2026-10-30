//! [`LocalFsCampaignStore`]: the campaign catalog on a filesystem. Not part of
//! the plan store: its four trees are written in one pass and read together by
//! `queue-maintenance`; neither that writer nor that pass touches campaigns.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use async_trait::async_trait;

use pipette_core::campaign::Campaign;
use pipette_core::tmp_file::TmpFileGuard;
use pipette_plan_types::CampaignId;
use pipette_store::{CampaignPage, CampaignStore};

use super::{page_flat_keyspace, remove_file_idempotent, run_blocking};

#[derive(Clone)]
pub(crate) struct LocalFsCampaignStore {
    campaigns_dir: PathBuf,
}

impl LocalFsCampaignStore {
    pub(crate) fn new(data_dir: &std::path::Path) -> Self {
        Self {
            campaigns_dir: data_dir.join("campaigns"),
        }
    }

    fn record_path(&self, campaign_id: &CampaignId) -> PathBuf {
        self.campaigns_dir
            .join(format!("{}.json", campaign_id.as_str()))
    }
}

#[async_trait]
impl CampaignStore for LocalFsCampaignStore {
    async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
        let path = self.record_path(campaign_id);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn put_campaign(&self, campaign: &Campaign) -> anyhow::Result<()> {
        let dir = self.campaigns_dir.clone();
        let path = self.record_path(&campaign.campaign_id);
        let bytes = serde_json::to_vec_pretty(campaign)?;
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Atomic replace: a reader racing a retarget never sees a truncated
            // record. The `.tmp-` name has no `.json` extension, so a listing
            // skips it.
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let guard = TmpFileGuard::new(tmp.clone());
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
            guard.disarm();
            Ok(())
        })
        .await
    }

    async fn list_campaigns(
        &self,
        after: Option<&CampaignId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage> {
        let dir = self.campaigns_dir.clone();
        let after = after.map(|id| id.as_str().to_string());
        run_blocking(move || {
            // No filter: a campaign has no lifecycle to narrow on, and ownership
            // is a caller's scope rather than a store-side selection.
            page_flat_keyspace(&dir, after.as_deref(), limit, CampaignId::try_new, |_| true)
        })
        .await
    }

    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
        remove_file_idempotent(self.record_path(campaign_id)).await
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::campaign::{CampaignOwner, NewCampaign};
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, PlanId, UserId};

    use super::*;

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
    async fn test_localfs_campaign_store_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_campaign_store_roundtrip(&LocalFsCampaignStore::new(
            dir.path(),
        ))
        .await
    }

    /// The path is built from one field, so a directory that moved is
    /// self-consistent and invisible to the round trip.
    #[tokio::test]
    async fn test_localfs_campaign_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsCampaignStore::new(dir.path());
        let campaign = a_campaign()?;
        store.put_campaign(&campaign).await?;
        assert!(
            dir.path()
                .join(format!("campaigns/{}.json", campaign.campaign_id))
                .exists(),
            "the record must land under campaigns/"
        );
        Ok(())
    }
}
