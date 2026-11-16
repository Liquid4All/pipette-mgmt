//! [`LocalFsCampaignStore`]: the campaign catalog on a filesystem. Not part of
//! the plan store: its four trees are written in one pass and read together by
//! `queue-maintenance`; neither that writer nor that pass touches campaigns.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use pipette_core::campaign::Campaign;
use pipette_core::tmp_file::TmpFileGuard;
use pipette_plan_types::CampaignId;
use pipette_store::{CampaignPage, CampaignStore};

use super::{page_flat_keyspace, run_blocking};

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
        run_blocking(move || write_record(&dir, &path, bytes)).await
    }

    /// Under an exclusive lock shared with other replaces and deletes, across
    /// processes too; a blind [`Self::put_campaign`] does not take it.
    async fn replace_campaign(&self, read: &Campaign, updated: &Campaign) -> anyhow::Result<bool> {
        let dir = self.campaigns_dir.clone();
        let path = self.record_path(&read.campaign_id);
        let (read, bytes) = (read.clone(), serde_json::to_vec_pretty(updated)?);
        run_blocking(move || {
            let _lock = lock(&dir)?;
            let stored: Campaign = match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            if stored != read {
                return Ok(false);
            }
            write_record(&dir, &path, bytes)?;
            Ok(true)
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

    /// Under the replace's lock, so a replace that read the record cannot
    /// write it back.
    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
        let dir = self.campaigns_dir.clone();
        let path = self.record_path(campaign_id);
        run_blocking(move || {
            let _lock = lock(&dir)?;
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }
}

/// The exclusive lock of `dir`, held until the file is dropped. Not on the
/// record, which a write renames over: a waiter would hold a lock on a file no
/// longer there. No `.json`, so a listing skips it.
fn lock(dir: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::File::options()
        .create(true)
        .append(true)
        .open(dir.join(".lock"))?;
    lock.lock()?;
    Ok(lock)
}

/// Atomic replace: a reader racing a retarget never sees a truncated record.
/// The `.tmp-` name has no `.json` extension, so a listing skips it.
fn write_record(dir: &Path, path: &Path, bytes: Vec<u8>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let guard = TmpFileGuard::new(tmp.clone());
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    guard.disarm();
    Ok(())
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

    #[tokio::test]
    async fn test_localfs_campaign_replace_is_conditional() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_campaign_replace_is_conditional(&LocalFsCampaignStore::new(
            dir.path(),
        ))
        .await
    }

    /// Writers racing from one read: the lock lets exactly one land, though
    /// each write renames a new file over the record.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_localfs_racing_campaign_replaces_land_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_racing_campaign_replaces_land_once(std::sync::Arc::new(
            LocalFsCampaignStore::new(dir.path()),
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
