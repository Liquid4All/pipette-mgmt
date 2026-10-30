//! [`CampaignStore`] on SQL: one row per campaign, the object stores' JSON body
//! minus `labels` and `state`, which are columns. No other typed column: one would round
//! `created_at` to microseconds or drop a field this build does not know.

use std::num::NonZeroUsize;

use anyhow::Context;
use async_trait::async_trait;

use pipette_core::campaign::Campaign;
use pipette_plan_types::{CampaignId, CampaignState, Labels};
use pipette_store::{CampaignPage, CampaignStore};

use crate::db::SqlDatabase;
use crate::listing::{self, Keyspace, Rows};

const CAMPAIGNS: Keyspace = Keyspace {
    table: "campaigns",
    id_column: "campaign_id",
    report_prefix: "campaigns/",
};

/// Every write is a whole-row upsert or a delete, so concurrent writers are
/// last-writer-wins, as on the object stores.
#[derive(Clone)]
pub(crate) struct SqlCampaignStore {
    db: SqlDatabase,
}

impl SqlCampaignStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl CampaignStore for SqlCampaignStore {
    async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
        let id = campaign_id.as_str();
        let row: Option<(String, String, String)> = self
            .db
            .fetch_optional("get_campaign", || {
                sqlx::query_as(
                    "SELECT body, labels::text, state FROM campaigns WHERE campaign_id = $1",
                )
                .bind(id)
            })
            .await?;
        row.map(|(body, labels, state)| {
            read_campaign(&body, &labels, &state).with_context(|| CAMPAIGNS.report_key(id))
        })
        .transpose()
    }

    async fn put_campaign(&self, campaign: &Campaign) -> anyhow::Result<()> {
        let (body, labels, state) = campaign_row(campaign)?;
        self.db
            .execute("put_campaign", || {
                sqlx::query(
                    "INSERT INTO campaigns (campaign_id, body, labels, state)
                     VALUES ($1, $2, $3::jsonb, $4)
                     ON CONFLICT (campaign_id) DO UPDATE
                     SET body = EXCLUDED.body, labels = EXCLUDED.labels, state = EXCLUDED.state",
                )
                .bind(campaign.campaign_id.as_str())
                .bind(body.as_str())
                .bind(labels.as_str())
                .bind(state)
            })
            .await?;
        Ok(())
    }

    async fn list_campaigns(
        &self,
        after: Option<&CampaignId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage> {
        // No filter: ownership is a caller's scope, not a store-side selection.
        listing::page_rows(
            self,
            CAMPAIGNS,
            after,
            limit,
            CampaignId::try_new,
            |_, (body, labels, state): (String, String, String)| {
                read_campaign(&body, &labels, &state)
            },
            |_| true,
        )
        .await
    }

    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
        // No row is success: a delete is idempotent.
        self.db
            .execute("delete_campaign", || {
                sqlx::query("DELETE FROM campaigns WHERE campaign_id = $1")
                    .bind(campaign_id.as_str())
            })
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Rows for SqlCampaignStore {
    /// A row's `body`, its `labels` as JSON text, and its `state`.
    type Row = (String, String, String);

    async fn rows_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<(String, Self::Row)>> {
        let rows: Vec<(String, String, String, String)> = listing::fetch_after(
            &self.db,
            CAMPAIGNS,
            "campaign_id, body, labels::text, state",
            after,
            limit,
        )
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, body, labels, state)| (id, (body, labels, state)))
            .collect())
    }

    async fn ids_after(&self, after: Option<&str>, limit: i64) -> anyhow::Result<Vec<String>> {
        let ids: Vec<(String,)> =
            listing::fetch_after(&self.db, CAMPAIGNS, CAMPAIGNS.id_column, after, limit).await?;
        Ok(ids.into_iter().map(|(id,)| id).collect())
    }
}

/// A `campaigns` row: the body without the labels and the state, the labels as
/// JSON, and the state. Pretty, as the object stores write it.
fn campaign_row(campaign: &Campaign) -> anyhow::Result<(String, String, &'static str)> {
    // An unlabelled, active record serializes neither key.
    let bare = Campaign {
        labels: Labels::default(),
        state: CampaignState::Active,
        ..campaign.clone()
    };
    let state = match campaign.state {
        CampaignState::Active => "active",
        CampaignState::Archived => "archived",
    };
    Ok((
        serde_json::to_string_pretty(&bare)?,
        serde_json::to_string(&campaign.labels)?,
        state,
    ))
}

fn read_campaign(body: &str, labels: &str, state: &str) -> anyhow::Result<Campaign> {
    Ok(Campaign {
        labels: serde_json::from_str(labels)?,
        state: state.parse().map_err(anyhow::Error::msg)?,
        ..serde_json::from_str(body)?
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::campaign::{CampaignOwner, NewCampaign};
    use pipette_core::types::Mint;
    use pipette_plan_types::{NonEmptyTrimmedString, PlanId, UserId};
    use pipette_store::drain_campaigns;

    use super::*;

    fn a_campaign(name: &str) -> anyhow::Result<Campaign> {
        Ok(NewCampaign::new(
            NonEmptyTrimmedString::try_new(name)?,
            CampaignOwner::User {
                user_id: UserId::mint(),
            },
        )
        .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Utc::now()))
    }

    async fn write_raw(db: &SqlDatabase, id: &str, body: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO campaigns (campaign_id, body) VALUES ($1, $2)")
            .bind(id)
            .bind(body)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    async fn stored_body(db: &SqlDatabase, id: &CampaignId) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT body FROM campaigns WHERE campaign_id = $1")
                .bind(id.as_str())
                .fetch_optional(db.pool())
                .await?,
        )
    }

    #[tokio::test]
    async fn meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_campaign_store_roundtrip(&SqlCampaignStore::new(
            test_db.db().clone(),
        ))
        .await
    }

    #[tokio::test]
    async fn the_body_is_the_object_stores_json() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignStore::new(test_db.db().clone());
        let campaign = a_campaign("Nightly")?;
        store.put_campaign(&campaign).await?;
        assert_eq!(
            stored_body(test_db.db(), &campaign.campaign_id).await?,
            Some(serde_json::to_string_pretty(&campaign)?)
        );
        Ok(())
    }

    /// The labels and the state are the columns' and not the body's, so a build
    /// older than the columns, which rewrites the body alone, keeps them.
    #[tokio::test]
    async fn labels_and_state_outlive_a_body_rewrite() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignStore::new(db.clone());
        let mut campaign = a_campaign("Nightly")?;
        campaign.set_labels(
            serde_json::from_value(serde_json::json!({"study": "acmelm-text", "device": "s26"}))?,
            Utc::now(),
        );
        campaign.set_state(CampaignState::Archived, Utc::now());
        store.put_campaign(&campaign).await?;
        let body = stored_body(db, &campaign.campaign_id)
            .await?
            .context("the campaign is stored")?;
        assert!(!body.contains("labels"), "{body}");
        assert!(!body.contains("state"), "{body}");

        sqlx::query(
            "INSERT INTO campaigns (campaign_id, body) VALUES ($1, $2)
             ON CONFLICT (campaign_id) DO UPDATE SET body = EXCLUDED.body",
        )
        .bind(campaign.campaign_id.as_str())
        .bind(body.as_str())
        .execute(db.pool())
        .await?;
        assert_eq!(
            store.get_campaign(&campaign.campaign_id).await?,
            Some(campaign.clone())
        );
        assert_eq!(drain_campaigns(&store).await?.items, vec![campaign]);
        Ok(())
    }

    /// A row whose id names no campaign is skipped without a report.
    #[tokio::test]
    async fn an_unreadable_row_is_reported_once_and_kept() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignStore::new(db.clone());
        let first = a_campaign("first")?;
        let broken = CampaignId::mint();
        let unknown_owner = CampaignId::mint();
        let last = a_campaign("last")?;
        store.put_campaign(&first).await?;
        store.put_campaign(&last).await?;
        write_raw(db, broken.as_str(), "{not json").await?;
        // An owner kind a later build might add.
        let future = serde_json::to_value(a_campaign("future")?)?;
        let mut future = future
            .as_object()
            .context("a campaign is an object")?
            .clone();
        future.insert("campaign_id".into(), unknown_owner.as_str().into());
        future.insert(
            "owner".into(),
            serde_json::json!({"type": "team", "team_id": "team_1"}),
        );
        let future = serde_json::to_string_pretty(&future)?;
        write_raw(db, unknown_owner.as_str(), &future).await?;
        // Sorts first, so it is examined on the first page.
        write_raw(db, "bogus", "{}").await?;

        let read = store.get_campaign(&broken).await;
        assert!(
            read.as_ref()
                .is_err_and(|e| e.root_cause().is::<serde_json::Error>()),
            "{read:?}"
        );
        assert!(store.get_campaign(&unknown_owner).await.is_err());

        let listing = drain_campaigns(&store).await?;
        assert_eq!(
            listing
                .items
                .iter()
                .map(|c| &c.campaign_id)
                .collect::<Vec<_>>(),
            vec![&first.campaign_id, &last.campaign_id]
        );
        let reported: Vec<&str> = listing.unreadable.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            reported,
            vec![
                format!("campaigns/{broken}.json"),
                format!("campaigns/{unknown_owner}.json"),
            ]
        );

        // Paged one row at a time, the reports ride on the page that passed
        // them, and the last full page still ends the drain.
        let page = store.list_campaigns(None, NonZeroUsize::MIN).await?;
        assert!(page.unreadable.is_empty());
        assert_eq!(page.resume_after, Some(first.campaign_id.clone()));
        let next = store
            .list_campaigns(page.resume_after.as_ref(), NonZeroUsize::MIN)
            .await?;
        assert_eq!(next.unreadable.len(), 2);
        assert_eq!(next.resume_after, None);

        assert_eq!(
            stored_body(db, &unknown_owner).await?,
            Some(future),
            "an unreadable row is never rewritten"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_writes_are_last_writer_wins() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignStore::new(test_db.db().clone());
        let base = a_campaign("race")?;
        crate::testing::assert_racing_writes_are_last_writer_wins(
            |n| {
                Ok(Campaign {
                    name: NonEmptyTrimmedString::try_new(format!("writer {n}"))?,
                    ..base.clone()
                })
            },
            |campaign: Campaign| {
                let store = store.clone();
                async move { store.put_campaign(&campaign).await }
            },
            || store.get_campaign(&base.campaign_id),
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_deletes_are_idempotent() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignStore::new(test_db.db().clone());
        let campaign = a_campaign("doomed")?;
        crate::testing::assert_racing_deletes_are_idempotent(
            campaign.clone(),
            |campaign: Campaign| {
                let store = store.clone();
                async move { store.put_campaign(&campaign).await }
            },
            |campaign: Campaign| {
                let store = store.clone();
                async move { store.delete_campaign(&campaign.campaign_id).await }
            },
            || store.get_campaign(&campaign.campaign_id),
        )
        .await
    }
}
