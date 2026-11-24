//! [`PlanTagStore`] on SQL: a `plan_tags` row per tag, keyed by the campaign and
//! the name, the tag's JSON in `body` so `created_at` keeps its precision.

use anyhow::Context;
use async_trait::async_trait;

use pipette_plan_types::{CampaignId, PlanTag};
use pipette_store::PlanTagStore;

use crate::db::SqlDatabase;

#[derive(Clone)]
pub(crate) struct SqlPlanTagStore {
    db: SqlDatabase,
}

impl SqlPlanTagStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PlanTagStore for SqlPlanTagStore {
    async fn create_tag(&self, tag: &PlanTag) -> anyhow::Result<bool> {
        let body = serde_json::to_string_pretty(tag)?;
        let written = self
            .db
            .execute("create_plan_tag", || {
                sqlx::query(
                    "INSERT INTO plan_tags (campaign_id, name, plan_id, body)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (campaign_id, name) DO NOTHING",
                )
                .bind(tag.campaign_id.as_str())
                .bind(tag.name.as_str())
                .bind(tag.plan_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(written == 1)
    }

    async fn campaign_tags(&self, campaign_ids: &[CampaignId]) -> anyhow::Result<Vec<PlanTag>> {
        let ids: Vec<&str> = campaign_ids.iter().map(CampaignId::as_str).collect();
        let rows: Vec<(String, String, String)> = self
            .db
            .fetch_all("campaign_plan_tags", || {
                sqlx::query_as(
                    "SELECT campaign_id, name, body FROM plan_tags WHERE campaign_id = ANY($1)",
                )
                .bind(&ids)
            })
            .await?;
        let mut tags = rows
            .into_iter()
            .map(|(campaign_id, name, body)| {
                serde_json::from_str::<PlanTag>(&body)
                    .with_context(|| format!("plan_tags/{campaign_id}/{name}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        tags.sort_by_key(|tag| std::cmp::Reverse(tag.created_at));
        Ok(tags)
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};

    use pipette_core::campaign::{CampaignOwner, NewCampaign};
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, OrgId, PlanId, UserId};
    use pipette_store::CampaignStore;

    use super::*;
    use crate::campaigns::SqlCampaignStore;

    /// A name is taken once per campaign, the listing is newest first, and a
    /// deleted campaign takes its tags with it.
    #[tokio::test]
    async fn a_tag_name_is_taken_once_and_goes_with_its_campaign() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let campaigns = SqlCampaignStore::new(test_db.db().clone());
        let tags = SqlPlanTagStore::new(test_db.db().clone());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let campaign = NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly")?,
            CampaignOwner::Org {
                org_id: OrgId::mint(),
            },
        )
        .around(plan_id.clone(), Timestamp::now());
        campaigns.put_campaign(&campaign).await?;
        let tag = |name: &str, minutes: i64| -> anyhow::Result<PlanTag> {
            Ok(PlanTag {
                campaign_id: campaign.campaign_id.clone(),
                name: NonEmptyTrimmedString::try_new(name)?,
                plan_id: plan_id.clone(),
                message: NonEmptyTrimmedString::try_new("shipped")?,
                tagger: Some(UserId::mint()),
                created_at: Timestamp::now() + SignedDuration::from_mins(minutes),
            })
        };
        let (first, second) = (tag("v1", 0)?, tag("v2", 1)?);

        assert!(tags.create_tag(&first).await?);
        assert!(tags.create_tag(&second).await?);
        assert!(!tags.create_tag(&tag("v1", 2)?).await?);
        assert_eq!(
            tags.campaign_tags(std::slice::from_ref(&campaign.campaign_id))
                .await?,
            vec![second, first]
        );

        campaigns.delete_campaign(&campaign.campaign_id).await?;
        assert!(
            tags.campaign_tags(&[campaign.campaign_id])
                .await?
                .is_empty()
        );
        Ok(())
    }
}
