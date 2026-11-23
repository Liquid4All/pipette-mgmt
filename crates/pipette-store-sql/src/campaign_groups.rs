//! [`CampaignGroupStore`] on SQL: a group's JSON in `campaign_groups`, a
//! `campaign_group_members` row per member, keyed by the campaign, with the
//! policy pass's JSON state, a policy's JSON in `campaign_group_policies`,
//! keyed by the group and kind, a `campaign_group_rerun_results` row per
//! re-run and kind that filed it, and a member's JSON state on each job of
//! its plan in `campaign_group_job_states`.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use anyhow::Context;
use async_trait::async_trait;
use jiff_sqlx::ToSqlx;
use sqlx::FromRow;

use pipette_core::campaign::{CampaignGroup, GroupPolicyState, JobPolicyState};
use pipette_plan_types::{
    CampaignGroupId, CampaignGroupPolicy, CampaignId, JobId, PlanId, PolicyRerun,
    PolicyRerunSummary,
};
use pipette_store::{
    CampaignGroupPage, CampaignGroupStore, CampaignInAnotherGroupError, EnabledPolicies,
    GroupMember, JobStateChanges, UnreadableObjectError,
};

use crate::db::SqlDatabase;
use crate::listing::{self, Keyspace};
use crate::retry::retry;

const GROUPS: Keyspace = Keyspace {
    table: "campaign_groups",
    id_column: "group_id",
    report_prefix: "campaign_groups/",
};

#[derive(Clone)]
pub(crate) struct SqlCampaignGroupStore {
    db: SqlDatabase,
}

impl SqlCampaignGroupStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl CampaignGroupStore for SqlCampaignGroupStore {
    async fn create_group(&self, group: &CampaignGroup) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(group)?;
        self.db
            .execute("create_campaign_group", || {
                sqlx::query("INSERT INTO campaign_groups (group_id, body) VALUES ($1, $2)")
                    .bind(group.group_id.as_str())
                    .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn update_group(&self, group: &CampaignGroup) -> anyhow::Result<bool> {
        let body = serde_json::to_string_pretty(group)?;
        let written = self
            .db
            .execute("update_campaign_group", || {
                sqlx::query("UPDATE campaign_groups SET body = $2 WHERE group_id = $1")
                    .bind(group.group_id.as_str())
                    .bind(body.as_str())
            })
            .await?;
        Ok(written > 0)
    }

    async fn get_group(&self, group_id: &CampaignGroupId) -> anyhow::Result<Option<CampaignGroup>> {
        let id = group_id.as_str();
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_campaign_group", || {
                sqlx::query_as("SELECT body FROM campaign_groups WHERE group_id = $1").bind(id)
            })
            .await?;
        body.map(|(body,)| serde_json::from_str(&body).with_context(|| GROUPS.report_key(id)))
            .transpose()
    }

    async fn list_groups(
        &self,
        after: Option<&CampaignGroupId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignGroupPage> {
        listing::page(
            &self.db,
            GROUPS,
            after,
            limit,
            CampaignGroupId::try_new,
            |_, body| serde_json::from_str(body),
            |_| true,
        )
        .await
    }

    async fn delete_group(&self, group_id: &CampaignGroupId) -> anyhow::Result<()> {
        self.db
            .execute("delete_campaign_group", || {
                sqlx::query("DELETE FROM campaign_groups WHERE group_id = $1")
                    .bind(group_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn add_members(
        &self,
        group_id: &CampaignGroupId,
        campaign_ids: &[CampaignId],
    ) -> anyhow::Result<()> {
        let ids: Vec<&str> = campaign_ids.iter().map(CampaignId::as_str).collect();
        // A racing add of a campaign to another group fails this insert, and
        // the retry then finds the campaign there.
        let taken: Option<(String, String)> = self
            .db
            .fetch_optional("add_campaign_group_members", || {
                sqlx::query_as(
                    "WITH taken AS (
                         SELECT campaign_id, group_id FROM campaign_group_members
                         WHERE campaign_id = ANY($1) AND group_id <> $2
                         ORDER BY campaign_id LIMIT 1
                     ), added AS (
                         INSERT INTO campaign_group_members (campaign_id, group_id)
                         SELECT unnest($1::text[]), $2 WHERE NOT EXISTS (SELECT FROM taken)
                         ON CONFLICT (campaign_id) DO NOTHING
                     )
                     SELECT campaign_id, group_id FROM taken",
                )
                .bind(ids.as_slice())
                .bind(group_id.as_str())
            })
            .await?;
        let Some((campaign_id, group_id)) = taken else {
            return Ok(());
        };
        Err(CampaignInAnotherGroupError {
            campaign_id: CampaignId::try_new(campaign_id)?,
            group_id: CampaignGroupId::try_new(group_id)?,
        }
        .into())
    }

    async fn remove_member(
        &self,
        group_id: &CampaignGroupId,
        campaign_id: &CampaignId,
    ) -> anyhow::Result<()> {
        self.db
            .execute("remove_campaign_group_member", || {
                sqlx::query(
                    "DELETE FROM campaign_group_members WHERE campaign_id = $1 AND group_id = $2",
                )
                .bind(campaign_id.as_str())
                .bind(group_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn members(&self, group_id: &CampaignGroupId) -> anyhow::Result<Vec<CampaignId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("campaign_group_members", || {
                sqlx::query_as(
                    "SELECT campaign_id FROM campaign_group_members WHERE group_id = $1
                     ORDER BY campaign_id",
                )
                .bind(group_id.as_str())
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id,)| CampaignId::try_new(id))
            .collect::<Result<_, _>>()?)
    }

    async fn group_of(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<CampaignGroupId>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("campaign_group_of", || {
                sqlx::query_as("SELECT group_id FROM campaign_group_members WHERE campaign_id = $1")
                    .bind(campaign_id.as_str())
            })
            .await?;
        Ok(row.map(|(id,)| CampaignGroupId::try_new(id)).transpose()?)
    }

    async fn group_and_member(
        &self,
        campaign_id: &CampaignId,
    ) -> anyhow::Result<Option<(CampaignGroup, GroupMember)>> {
        let row: Option<(String, String, Option<String>)> = self
            .db
            .fetch_optional("campaign_group_and_member", || {
                sqlx::query_as(
                    "SELECT g.group_id, g.body, m.policy_state
                     FROM campaign_group_members m
                     JOIN campaign_groups g ON g.group_id = m.group_id
                     WHERE m.campaign_id = $1",
                )
                .bind(campaign_id.as_str())
            })
            .await?;
        row.map(|(id, body, policy_state)| {
            let group = serde_json::from_str(&body).with_context(|| GROUPS.report_key(&id))?;
            Ok((group, member((campaign_id.to_string(), policy_state))?))
        })
        .transpose()
    }

    async fn list_policies(
        &self,
        group_id: &CampaignGroupId,
    ) -> anyhow::Result<Vec<CampaignGroupPolicy>> {
        let id = group_id.as_str();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("campaign_group_policies", || {
                sqlx::query_as(
                    "SELECT kind, body FROM campaign_group_policies WHERE group_id = $1
                     ORDER BY kind",
                )
                .bind(id)
            })
            .await?;
        rows.into_iter()
            .map(|(kind, body)| {
                serde_json::from_str(&body)
                    .with_context(|| format!("campaign_group_policies/{id}/{kind}"))
            })
            .collect()
    }

    async fn replace_policy(
        &self,
        read: &CampaignGroupPolicy,
        updated: &CampaignGroupPolicy,
    ) -> anyhow::Result<bool> {
        let (id, kind) = (read.group_id.as_str(), read.kind.as_ref());
        let row: Option<(String,)> = self
            .db
            .fetch_optional("replace_campaign_group_policy_read", || {
                sqlx::query_as(
                    "SELECT body FROM campaign_group_policies WHERE group_id = $1 AND kind = $2",
                )
                .bind(id)
                .bind(kind)
            })
            .await?;
        let body = serde_json::to_string_pretty(updated)?;
        let written = match row {
            Some((stored,)) => {
                let decoded: CampaignGroupPolicy = serde_json::from_str(&stored)
                    .with_context(|| format!("campaign_group_policies/{id}/{kind}"))?;
                if decoded != *read {
                    return Ok(false);
                }
                // The row as read, not `read` serialized: a body an older build
                // wrote can hold a field this one drops.
                self.db
                    .execute("replace_campaign_group_policy", || {
                        sqlx::query(
                            "UPDATE campaign_group_policies SET body = $3
                             WHERE group_id = $1 AND kind = $2 AND body = $4",
                        )
                        .bind(id)
                        .bind(kind)
                        .bind(body.as_str())
                        .bind(stored.as_str())
                    })
                    .await?
            }
            None if *read == read.kind.defaults(read.group_id.clone()) => {
                self.db
                    .execute("insert_campaign_group_policy", || {
                        sqlx::query(
                            "INSERT INTO campaign_group_policies (group_id, kind, body)
                             VALUES ($1, $2, $3) ON CONFLICT (group_id, kind) DO NOTHING",
                        )
                        .bind(id)
                        .bind(kind)
                        .bind(body.as_str())
                    })
                    .await?
            }
            None => return Ok(false),
        };
        Ok(written == 1)
    }

    async fn enabled_policies(&self) -> anyhow::Result<EnabledPolicies> {
        // `body` is text, not jsonb, so `enabled` is checked after the decode.
        let rows: Vec<(String, String, String)> = self
            .db
            .fetch_all("enabled_campaign_group_policies", || {
                sqlx::query_as(
                    "SELECT group_id, kind, body FROM campaign_group_policies
                     ORDER BY group_id, kind",
                )
            })
            .await?;
        let stored: BTreeSet<String> = rows.iter().map(|(group_id, ..)| group_id.clone()).collect();
        let mut enabled = rows.into_iter().try_fold(
            EnabledPolicies::default(),
            |mut enabled, (group_id, kind, body)| {
                // An older binary can store failed on without codes, which flags nothing.
                let read = serde_json::from_str::<CampaignGroupPolicy>(&body)
                    .map_err(anyhow::Error::from)
                    .and_then(|policy| {
                        if policy.enabled {
                            policy.kind.check_settings(&policy.settings)?;
                        }
                        anyhow::Ok(policy)
                    });
                match read {
                    Ok(policy) if policy.enabled => enabled.policies.push(policy),
                    Ok(_) => {}
                    Err(e) => enabled.unreadable.push((
                        CampaignGroupId::try_new(group_id.clone())?,
                        UnreadableObjectError {
                            key: format!("campaign_group_policies/{group_id}/{kind}"),
                            reason: e.to_string(),
                        },
                    )),
                }
                anyhow::Ok(enabled)
            },
        )?;
        let not_off: BTreeSet<&str> = enabled
            .policies
            .iter()
            .map(|policy| policy.group_id.as_str())
            .chain(
                enabled
                    .unreadable
                    .iter()
                    .map(|(group_id, _)| group_id.as_str()),
            )
            .collect();
        enabled.off = stored
            .into_iter()
            .filter(|group_id| !not_off.contains(group_id.as_str()))
            .map(CampaignGroupId::try_new)
            .collect::<Result<_, _>>()?;
        Ok(enabled)
    }

    async fn policy_states(&self, group_id: &CampaignGroupId) -> anyhow::Result<Vec<GroupMember>> {
        let rows: Vec<(String, Option<String>)> = self
            .db
            .fetch_all("campaign_group_policy_states", || {
                sqlx::query_as(
                    "SELECT campaign_id, policy_state FROM campaign_group_members
                     WHERE group_id = $1 ORDER BY campaign_id",
                )
                .bind(group_id.as_str())
            })
            .await?;
        rows.into_iter().map(member).collect()
    }

    async fn member_states(&self, campaign_ids: &[CampaignId]) -> anyhow::Result<Vec<GroupMember>> {
        let ids: Vec<&str> = campaign_ids.iter().map(CampaignId::as_str).collect();
        let rows: Vec<(String, Option<String>)> = self
            .db
            .fetch_all("campaign_group_member_states", || {
                sqlx::query_as(
                    "SELECT campaign_id, policy_state FROM campaign_group_members
                     WHERE campaign_id = ANY($1) ORDER BY campaign_id",
                )
                .bind(&ids)
            })
            .await?;
        rows.into_iter().map(member).collect()
    }

    async fn set_policy_state(
        &self,
        group_id: &CampaignGroupId,
        campaign_id: &CampaignId,
        state: &GroupPolicyState,
        job_states: &JobStateChanges,
    ) -> anyhow::Result<bool> {
        let body = serde_json::to_string(state)?;
        let put_jobs: Vec<&str> = job_states.put.iter().map(|v| v.job_id.as_str()).collect();
        let put_bodies = job_states
            .put
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<String>, _>>()?;
        let removed: Vec<&str> = job_states.removed.iter().map(JobId::as_str).collect();
        let (db, campaign, group) = (&self.db, campaign_id.as_str(), group_id.as_str());
        let (body, put_jobs, put_bodies, removed) = (&body, &put_jobs, &put_bodies, &removed);
        retry("set_campaign_group_policy_state", move || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                if !write_member_state(&mut tx, campaign, group, body).await? {
                    return Ok(false);
                }
                if !removed.is_empty() {
                    sqlx::query(
                        "DELETE FROM campaign_group_job_states
                         WHERE campaign_id = $1 AND job_id = ANY($2)",
                    )
                    .bind(campaign)
                    .bind(removed)
                    .execute(&mut *tx)
                    .await?;
                }
                if !put_jobs.is_empty() {
                    sqlx::query(
                        "INSERT INTO campaign_group_job_states (campaign_id, job_id, body)
                         SELECT $1, * FROM UNNEST($2::text[], $3::text[])
                         ON CONFLICT (campaign_id, job_id) DO UPDATE SET body = EXCLUDED.body",
                    )
                    .bind(campaign)
                    .bind(put_jobs)
                    .bind(put_bodies)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(true)
            })
        })
        .await
    }

    async fn clear_policy_state(
        &self,
        group_id: &CampaignGroupId,
        campaign_id: &CampaignId,
        state: &GroupPolicyState,
    ) -> anyhow::Result<bool> {
        let body = serde_json::to_string(state)?;
        let (db, campaign, group, body) =
            (&self.db, campaign_id.as_str(), group_id.as_str(), &body);
        retry("clear_campaign_group_policy_state", move || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                if !write_member_state(&mut tx, campaign, group, body).await? {
                    return Ok(false);
                }
                sqlx::query("DELETE FROM campaign_group_job_states WHERE campaign_id = $1")
                    .bind(campaign)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(true)
            })
        })
        .await
    }

    async fn job_states(
        &self,
        group_id: &CampaignGroupId,
        campaign_id: &CampaignId,
    ) -> anyhow::Result<Vec<JobPolicyState>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("campaign_group_job_states", || {
                sqlx::query_as(
                    "SELECT v.job_id, v.body FROM campaign_group_job_states v
                     JOIN campaign_group_members m USING (campaign_id)
                     WHERE v.campaign_id = $1 AND m.group_id = $2 ORDER BY v.job_id",
                )
                .bind(campaign_id.as_str())
                .bind(group_id.as_str())
            })
            .await?;
        rows.into_iter()
            .map(|(job_id, body)| {
                serde_json::from_str(&body).with_context(|| {
                    format!("campaign_group_job_states/{campaign_id}/{job_id} does not decode")
                })
            })
            .collect()
    }

    async fn record_reruns(
        &self,
        group_id: &CampaignGroupId,
        reruns: &[PolicyRerun],
    ) -> anyhow::Result<usize> {
        if reruns.is_empty() {
            return Ok(0);
        }
        let text =
            |pick: fn(&PolicyRerun) -> &str| -> Vec<&str> { reruns.iter().map(pick).collect() };
        let (campaigns, kinds, jobs, ofs, plans, benchmarks) = (
            text(|rerun| rerun.campaign_id.as_str()),
            text(|rerun| rerun.kind.as_ref()),
            text(|rerun| rerun.rerun_job_id.as_str()),
            text(|rerun| rerun.rerun_of.as_str()),
            text(|rerun| rerun.plan_id.as_str()),
            text(|rerun| rerun.benchmark_id.as_str()),
        );
        let filed: Vec<jiff_sqlx::Timestamp> =
            reruns.iter().map(|r| r.filed_at.to_sqlx()).collect();
        let attempts = reruns
            .iter()
            .map(|rerun| i32::try_from(rerun.attempt))
            .collect::<Result<Vec<i32>, _>>()?;
        let models: Vec<Option<&str>> = reruns.iter().map(|r| r.model_key.as_deref()).collect();
        let befores: Vec<f64> = reruns.iter().map(|rerun| rerun.before).collect();
        let thresholds: Vec<Option<f64>> = reruns.iter().map(|rerun| rerun.threshold).collect();
        let short_ms: Vec<Option<f64>> = reruns.iter().map(|r| r.short_repetition_ms).collect();
        let short_percents: Vec<Option<f64>> =
            reruns.iter().map(|r| r.short_repetition_percent).collect();
        let codes = reruns
            .iter()
            .map(|rerun| rerun.codes.as_ref().map(serde_json::to_string).transpose())
            .collect::<Result<Vec<Option<String>>, _>>()?;
        let before_codes: Vec<Option<&str>> =
            reruns.iter().map(|r| r.before_code.as_deref()).collect();
        let (afters, after_codes, outcomes, judged) = judgements(reruns);
        let written = self
            .db
            .execute("record_campaign_group_reruns", || {
                sqlx::query(
                    "INSERT INTO campaign_group_rerun_results (group_id, campaign_id, kind,
                         rerun_job_id, rerun_of, plan_id, filed_at, attempt, benchmark_id,
                         model_key, before, threshold, before_code, after, after_code, outcome,
                         judged_at, short_repetition_ms, short_repetition_percent, codes)
                     SELECT $1, * FROM unnest($2::text[], $3::text[], $4::text[], $5::text[],
                         $6::text[], $7::timestamptz[], $8::integer[], $9::text[], $10::text[],
                         $11::float8[], $12::float8[], $13::text[], $14::float8[], $15::text[],
                         $16::text[], $17::timestamptz[], $18::float8[], $19::float8[],
                         $20::text[])
                     ON CONFLICT (group_id, rerun_job_id, kind) DO NOTHING",
                )
                .bind(group_id.as_str())
                .bind(&campaigns)
                .bind(&kinds)
                .bind(&jobs)
                .bind(&ofs)
                .bind(&plans)
                .bind(&filed)
                .bind(&attempts)
                .bind(&benchmarks)
                .bind(&models)
                .bind(&befores)
                .bind(&thresholds)
                .bind(&before_codes)
                .bind(&afters)
                .bind(&after_codes)
                .bind(&outcomes)
                .bind(&judged)
                .bind(&short_ms)
                .bind(&short_percents)
                .bind(&codes)
            })
            .await?;
        Ok(usize::try_from(written)?)
    }

    async fn waiting_reruns(
        &self,
        group_id: &CampaignGroupId,
        campaign_id: &CampaignId,
    ) -> anyhow::Result<Vec<PolicyRerun>> {
        let rows: Vec<RerunRow> = self
            .db
            .fetch_all("waiting_campaign_group_reruns", || {
                sqlx::query_as(
                    "SELECT * FROM campaign_group_rerun_results
                     WHERE group_id = $1 AND campaign_id = $2 AND judged_at IS NULL
                     ORDER BY rerun_job_id, kind",
                )
                .bind(group_id.as_str())
                .bind(campaign_id.as_str())
            })
            .await?;
        rows.into_iter().map(RerunRow::parse).collect()
    }

    async fn judge_reruns(
        &self,
        group_id: &CampaignGroupId,
        judged: &[PolicyRerun],
    ) -> anyhow::Result<usize> {
        if judged.is_empty() {
            return Ok(0);
        }
        let jobs: Vec<&str> = judged.iter().map(|r| r.rerun_job_id.as_str()).collect();
        let kinds: Vec<&str> = judged.iter().map(|rerun| rerun.kind.as_ref()).collect();
        let (afters, after_codes, outcomes, at) = judgements(judged);
        let written = self
            .db
            .execute("judge_campaign_group_reruns", || {
                sqlx::query(
                    "UPDATE campaign_group_rerun_results r
                     SET after = j.after, after_code = j.after_code, outcome = j.outcome,
                         judged_at = j.judged_at
                     FROM unnest($2::text[], $3::text[], $4::float8[], $5::text[], $6::text[],
                         $7::timestamptz[])
                         AS j (rerun_job_id, kind, after, after_code, outcome, judged_at)
                     WHERE r.group_id = $1 AND r.rerun_job_id = j.rerun_job_id
                         AND r.kind = j.kind AND r.judged_at IS NULL",
                )
                .bind(group_id.as_str())
                .bind(&jobs)
                .bind(&kinds)
                .bind(&afters)
                .bind(&after_codes)
                .bind(&outcomes)
                .bind(&at)
            })
            .await?;
        Ok(usize::try_from(written)?)
    }

    async fn rerun_results(
        &self,
        group_id: &CampaignGroupId,
        campaigns: &[CampaignId],
        offset: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<(Vec<PolicyRerunSummary>, Vec<PolicyRerun>)> {
        let ids: Vec<&str> = campaigns.iter().map(CampaignId::as_str).collect();
        let sums: Vec<SummaryRow> = self
            .db
            .fetch_all("campaign_group_rerun_summary", || {
                sqlx::query_as(
                    "SELECT kind, count(*), count(*) FILTER (WHERE judged_at IS NULL),
                         count(*) FILTER (WHERE outcome = 'cleared'),
                         count(*) FILTER (WHERE outcome = 'flagged'),
                         count(*) FILTER (WHERE outcome = 'failed'),
                         count(*) FILTER (WHERE outcome = 'dropped'),
                         percentile_cont(0.5) WITHIN GROUP (ORDER BY before)
                             FILTER (WHERE outcome IN ('cleared', 'flagged', 'failed')),
                         percentile_cont(0.5) WITHIN GROUP (ORDER BY after)
                             FILTER (WHERE outcome IN ('cleared', 'flagged', 'failed'))
                     FROM campaign_group_rerun_results
                     WHERE group_id = $1 AND campaign_id = ANY($2)
                     GROUP BY kind",
                )
                .bind(group_id.as_str())
                .bind(ids.as_slice())
            })
            .await?;
        let mut summary = sums
            .into_iter()
            .map(
                |(kind, reruns, waiting, cleared, flagged, failed, dropped, before, after)| {
                    anyhow::Ok(PolicyRerunSummary {
                        kind: named(kind)?,
                        reruns: usize::try_from(reruns)?,
                        waiting: usize::try_from(waiting)?,
                        cleared: usize::try_from(cleared)?,
                        flagged: usize::try_from(flagged)?,
                        failed: usize::try_from(failed)?,
                        dropped: usize::try_from(dropped)?,
                        median_before: before,
                        median_after: after,
                    })
                },
            )
            .collect::<anyhow::Result<Vec<_>>>()?;
        summary.sort_by_key(|kind| kind.kind);
        let (offset, limit) = (
            i64::try_from(offset)?,
            limit.map(i64::try_from).transpose()?,
        );
        let rows: Vec<RerunRow> = self
            .db
            .fetch_all("campaign_group_rerun_results", || {
                sqlx::query_as(
                    "SELECT * FROM campaign_group_rerun_results
                     WHERE group_id = $1 AND campaign_id = ANY($2)
                     ORDER BY filed_at DESC, rerun_job_id, kind OFFSET $3 LIMIT $4",
                )
                .bind(group_id.as_str())
                .bind(ids.as_slice())
                .bind(offset)
                .bind(limit)
            })
            .await?;
        let rows = rows
            .into_iter()
            .map(RerunRow::parse)
            .collect::<anyhow::Result<_>>()?;
        Ok((summary, rows))
    }
}

/// One kind's counts and medians, as the summary query reads them.
type SummaryRow = (
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<f64>,
    Option<f64>,
);

/// The `after`, `after_code`, `outcome` and `judged_at` columns of rows.
type Judgements<'r> = (
    Vec<Option<f64>>,
    Vec<Option<&'r str>>,
    Vec<Option<&'r str>>,
    Vec<Option<jiff_sqlx::Timestamp>>,
);

fn judgements(reruns: &[PolicyRerun]) -> Judgements<'_> {
    (
        reruns.iter().map(|rerun| rerun.after).collect(),
        reruns.iter().map(|r| r.after_code.as_deref()).collect(),
        reruns
            .iter()
            .map(|rerun| rerun.outcome.as_ref().map(|outcome| outcome.as_ref()))
            .collect(),
        reruns
            .iter()
            .map(|r| r.judged_at.map(ToSqlx::to_sqlx))
            .collect(),
    )
}

/// A `campaign_group_rerun_results` row before the store parses it.
#[derive(FromRow)]
struct RerunRow {
    campaign_id: String,
    kind: String,
    rerun_job_id: String,
    rerun_of: String,
    plan_id: String,
    filed_at: jiff_sqlx::Timestamp,
    attempt: i32,
    benchmark_id: String,
    model_key: Option<String>,
    before: f64,
    threshold: Option<f64>,
    short_repetition_ms: Option<f64>,
    short_repetition_percent: Option<f64>,
    codes: Option<String>,
    before_code: Option<String>,
    after: Option<f64>,
    after_code: Option<String>,
    outcome: Option<String>,
    judged_at: Option<jiff_sqlx::Timestamp>,
}

impl RerunRow {
    fn parse(self) -> anyhow::Result<PolicyRerun> {
        Ok(PolicyRerun {
            campaign_id: CampaignId::try_new(self.campaign_id)?,
            kind: named(self.kind)?,
            rerun_job_id: JobId::try_new(self.rerun_job_id)?,
            rerun_of: JobId::try_new(self.rerun_of)?,
            plan_id: PlanId::try_new(self.plan_id)?,
            filed_at: self.filed_at.to_jiff(),
            attempt: u32::try_from(self.attempt)?,
            benchmark_id: self.benchmark_id,
            model_key: self.model_key,
            before: self.before,
            threshold: self.threshold,
            short_repetition_ms: self.short_repetition_ms,
            short_repetition_percent: self.short_repetition_percent,
            codes: self
                .codes
                .map(|codes| serde_json::from_str(&codes))
                .transpose()?,
            before_code: self.before_code,
            after: self.after,
            after_code: self.after_code,
            outcome: self.outcome.map(named).transpose()?,
            judged_at: self.judged_at.map(jiff_sqlx::Timestamp::to_jiff),
        })
    }
}

/// A `snake_case` name as the type it names reads it.
fn named<T: serde::de::DeserializeOwned>(name: String) -> anyhow::Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(name))?)
}

/// A `campaign_group_members` row's campaign and policy state.
fn member((campaign_id, policy_state): (String, Option<String>)) -> anyhow::Result<GroupMember> {
    let policy_state = policy_state
        .map(|state| serde_json::from_str(&state))
        .transpose()
        .map_err(|e| UnreadableObjectError {
            key: format!("campaign_group_members/{campaign_id}"),
            reason: e.to_string(),
        });
    Ok(GroupMember {
        campaign_id: CampaignId::try_new(campaign_id)?,
        policy_state,
    })
}

/// Write a member's policy state `body` in `tx`; `false` when the campaign is
/// not a member of `group`, as after a move to another group.
async fn write_member_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    campaign: &str,
    group: &str,
    body: &str,
) -> Result<bool, sqlx::Error> {
    let written = sqlx::query(
        "UPDATE campaign_group_members SET policy_state = $3
         WHERE campaign_id = $1 AND group_id = $2",
    )
    .bind(campaign)
    .bind(group)
    .bind(body)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(written > 0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use jiff::{SignedDuration, Timestamp};

    use pipette_core::campaign::{CampaignOwner, GroupFiling, NewCampaign, Refusal};
    use pipette_core::types::Mint;
    use pipette_plan_types::{
        FailureCode, OrgId, PolicyDeferral, PolicyKind, PolicySettings, RerunOutcome, UserId,
    };
    use pipette_store::CampaignStore;

    use super::*;
    use crate::campaigns::SqlCampaignStore;

    fn an_owner() -> CampaignOwner {
        CampaignOwner::Org {
            org_id: OrgId::mint(),
        }
    }

    fn a_group(name: &str) -> anyhow::Result<CampaignGroup> {
        Ok(CampaignGroup::new(
            name.parse()?,
            an_owner(),
            Timestamp::now(),
        ))
    }

    async fn a_stored_group(store: &SqlCampaignGroupStore) -> anyhow::Result<CampaignGroupId> {
        let group = a_group("study")?;
        store.create_group(&group).await?;
        Ok(group.group_id)
    }

    async fn a_campaign(db: &SqlDatabase) -> anyhow::Result<CampaignId> {
        let campaign = NewCampaign::new("nightly".parse()?, an_owner())
            .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Timestamp::now());
        SqlCampaignStore::new(db.clone())
            .put_campaign(&campaign)
            .await?;
        Ok(campaign.campaign_id)
    }

    #[tokio::test]
    async fn a_group_is_created_listed_renamed_and_deleted() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignGroupStore::new(test_db.db().clone());
        let (first, second) = (a_group("s26")?, a_group("pixel")?);
        store.create_group(&first).await?;
        store.create_group(&second).await?;
        let listed = store.list_groups(None, NonZeroUsize::MAX).await?.items;
        assert_eq!(listed, [first.clone(), second]);

        let id = &first.group_id;
        let at = first.updated_at + SignedDuration::from_secs(5);
        let renamed = CampaignGroup {
            name: "s26 ultra".parse()?,
            updated_at: at,
            ..first.clone()
        };
        let taken = store.create_group(&renamed).await;
        assert!(taken.is_err(), "{taken:?}");
        assert_eq!(store.get_group(id).await?.as_ref(), Some(&first));

        let mut group = store.get_group(id).await?.context("the group is stored")?;
        assert!(group.rename(renamed.name.clone(), at));
        // The same name again changes nothing, so `updated_at` stays.
        assert!(!group.rename(renamed.name.clone(), at + SignedDuration::from_secs(5)));
        assert!(store.update_group(&group).await?);
        assert_eq!(store.get_group(id).await?, Some(renamed));

        store.delete_group(id).await?;
        store.delete_group(id).await?;
        assert!(!store.update_group(&group).await?);
        assert_eq!(store.get_group(id).await?, None);
        Ok(())
    }

    /// A campaign of another group refuses the whole add, one of this group
    /// adds nothing, a delete on either side deletes only the membership, and a
    /// group or campaign that does not exist fails an add that names a campaign.
    #[tokio::test]
    async fn a_campaign_is_in_at_most_one_group() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let campaigns = SqlCampaignStore::new(db.clone());
        let (first, second) = (a_stored_group(&store).await?, a_stored_group(&store).await?);
        let (a, b, c) = (
            a_campaign(db).await?,
            a_campaign(db).await?,
            a_campaign(db).await?,
        );

        store.add_members(&first, &[b.clone(), a.clone()]).await?;
        store.add_members(&first, std::slice::from_ref(&a)).await?;
        assert_eq!(store.members(&first).await?, [a.clone(), b.clone()]);
        assert_eq!(store.group_of(&a).await?, Some(first.clone()));

        let refused = store.add_members(&second, &[c.clone(), b.clone()]).await;
        let refusal = refused
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<CampaignInAnotherGroupError>())
            .map(|e| (&e.campaign_id, &e.group_id));
        assert_eq!(refusal, Some((&b, &first)), "{refused:?}");
        assert_eq!(store.group_of(&c).await?, None);

        store.remove_member(&second, &b).await?;
        store.remove_member(&first, &a).await?;
        assert_eq!(store.members(&first).await?, std::slice::from_ref(&b));

        store.add_members(&second, &[a.clone(), c.clone()]).await?;
        campaigns.delete_campaign(&c).await?;
        assert_eq!(store.members(&second).await?, [a]);
        store.delete_group(&first).await?;
        assert_eq!(store.group_of(&b).await?, None);
        assert!(campaigns.get_campaign(&b).await?.is_some());

        let deleted_group = store.add_members(&first, std::slice::from_ref(&b)).await;
        assert!(deleted_group.is_err(), "{deleted_group:?}");
        store.add_members(&first, &[]).await?;
        let unknown_campaign = store
            .add_members(&second, &[b.clone(), CampaignId::mint()])
            .await;
        assert!(unknown_campaign.is_err(), "{unknown_campaign:?}");
        assert_eq!(store.group_of(&b).await?, None);
        Ok(())
    }

    fn turned_on(policy: &CampaignGroupPolicy) -> CampaignGroupPolicy {
        CampaignGroupPolicy {
            enabled: true,
            enabled_by: Some(UserId::mint()),
            enabled_at: Some(Timestamp::now()),
            ..policy.clone()
        }
    }

    /// A decided job state of `job`, which `flagged` flag.
    fn a_job_state(job: &str, flagged: &[PolicyKind]) -> anyhow::Result<JobPolicyState> {
        Ok(JobPolicyState {
            job_id: JobId::try_new(job)?,
            flagged: flagged.iter().copied().collect(),
            decided: true,
            missing_params: BTreeSet::new(),
            family: None,
            failed: false,
        })
    }

    /// A replace writes the group's policy of its kind and keeps the others,
    /// and deleting the group deletes its policies alone.
    #[tokio::test]
    async fn a_group_keeps_one_policy_per_kind() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignGroupStore::new(test_db.db().clone());
        let (group, other) = (a_stored_group(&store).await?, a_stored_group(&store).await?);
        let spread = PolicyKind::Spread.defaults(group.clone());
        let swap_off = PolicyKind::Swap.defaults(group.clone());
        let swap = turned_on(&swap_off);
        let failed = PolicyKind::Failed.defaults(other.clone());
        assert!(store.replace_policy(&swap_off, &swap).await?);
        assert!(store.replace_policy(&spread, &spread).await?);
        assert!(store.replace_policy(&failed, &failed).await?);
        assert!(store.replace_policy(&swap, &swap_off).await?);
        assert_eq!(
            store.list_policies(&group).await?,
            [spread, swap_off.clone()]
        );

        store.delete_group(&group).await?;
        assert!(store.list_policies(&group).await?.is_empty());
        assert_eq!(store.list_policies(&other).await?, [failed]);
        let orphan = store.replace_policy(&swap_off, &swap).await;
        assert!(orphan.is_err(), "{orphan:?}");
        Ok(())
    }

    /// A replace lands only on the policy as read: the stored row, or the
    /// kind's defaults while none is stored. A row holding a field this build
    /// drops compares without it, and the write leaves it out.
    #[tokio::test]
    async fn a_policy_replace_lands_only_on_the_policy_as_read() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let group = a_stored_group(&store).await?;
        let defaults = PolicyKind::Spread.defaults(group.clone());
        let on = turned_on(&defaults);
        let raised = CampaignGroupPolicy {
            max_reruns: 15,
            ..on.clone()
        };

        assert!(!store.replace_policy(&on, &raised).await?, "nothing stored");
        assert!(store.replace_policy(&defaults, &on).await?);
        assert!(!store.replace_policy(&defaults, &raised).await?, "stale");
        assert!(store.replace_policy(&on, &raised).await?);
        assert!(!store.replace_policy(&on, &defaults).await?, "stale");
        assert_eq!(
            store.list_policies(&group).await?,
            std::slice::from_ref(&raised)
        );

        // A row an older build wrote with its `on_limit`.
        let mut older = serde_json::to_value(&raised)?;
        older
            .as_object_mut()
            .context("a policy is an object")?
            .insert("on_limit".into(), "fail".into());
        sqlx::query("UPDATE campaign_group_policies SET body = $1 WHERE group_id = $2")
            .bind(serde_json::to_string_pretty(&older)?)
            .bind(group.as_str())
            .execute(db.pool())
            .await?;
        assert!(store.replace_policy(&raised, &on).await?);
        let body: String =
            sqlx::query_scalar("SELECT body FROM campaign_group_policies WHERE group_id = $1")
                .bind(group.as_str())
                .fetch_one(db.pool())
                .await?;
        assert_eq!(body, serde_json::to_string_pretty(&on)?);
        Ok(())
    }

    /// Racing replaces from one read land once, from the defaults and from a
    /// stored row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_policy_replaces_from_one_read_land_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignGroupStore::new(test_db.db().clone());
        let group = a_stored_group(&store).await?;
        let first = race_replaces(&store, &PolicyKind::Failed.defaults(group)).await?;
        race_replaces(&store, &first).await?;
        Ok(())
    }

    /// Race replaces of `read`, each to its own re-run count above `read`'s:
    /// exactly one lands, and the group then holds it.
    async fn race_replaces(
        store: &SqlCampaignGroupStore,
        read: &CampaignGroupPolicy,
    ) -> anyhow::Result<CampaignGroupPolicy> {
        let landed = crate::testing::race(|n| {
            let (store, read) = (store.clone(), read.clone());
            async move {
                let updated = CampaignGroupPolicy {
                    max_reruns: read.max_reruns + 1 + u32::try_from(n)?,
                    ..read.clone()
                };
                Ok(store
                    .replace_policy(&read, &updated)
                    .await?
                    .then_some(updated))
            }
        })
        .await?;
        let landed: Vec<CampaignGroupPolicy> = landed.into_iter().flatten().collect();
        let [winner] = &landed[..] else {
            anyhow::bail!("{} of the replaces landed: {landed:?}", landed.len());
        };
        assert_eq!(
            store.list_policies(&read.group_id).await?,
            std::slice::from_ref(winner)
        );
        Ok(winner.clone())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_groups_add_a_campaign_to_exactly_one() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCampaignGroupStore::new(test_db.db().clone());
        let campaign = a_campaign(test_db.db()).await?;
        let groups = (0..crate::testing::RACERS).map(|_| a_stored_group(&store));
        let groups = futures::future::try_join_all(groups).await?;
        let added = crate::testing::race(|n| {
            let (store, group, campaign) = (store.clone(), groups[n].clone(), campaign.clone());
            async move {
                match store.add_members(&group, &[campaign]).await {
                    Ok(()) => Ok(true),
                    Err(e) if e.is::<CampaignInAnotherGroupError>() => Ok(false),
                    Err(e) => Err(e),
                }
            }
        })
        .await?;
        let winners: Vec<_> = groups
            .into_iter()
            .zip(added)
            .filter_map(|(group, added)| added.then_some(group))
            .collect();
        assert_eq!(winners.len(), 1, "{winners:?}");
        assert_eq!(store.group_of(&campaign).await?.as_ref(), winners.first());
        Ok(())
    }

    /// Every group's policies that are on. A row that does not decode, or a
    /// policy on whose settings its kind refuses, is reported with its group,
    /// whose other policies still read.
    #[tokio::test]
    async fn enabled_policies_report_a_row_that_does_not_decode_or_check() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let (first, second, broken, uncoded, idle) = (
            a_stored_group(&store).await?,
            a_stored_group(&store).await?,
            a_stored_group(&store).await?,
            a_stored_group(&store).await?,
            a_stored_group(&store).await?,
        );
        // Stored, and off.
        let idle_swap = PolicyKind::Swap.defaults(idle.clone());
        assert!(store.replace_policy(&idle_swap, &idle_swap).await?);
        let policies = [
            PolicyKind::Swap.defaults(first.clone()),
            PolicyKind::Spread.defaults(first.clone()),
            PolicyKind::Failed.defaults(second),
            PolicyKind::TtftOverE2e.defaults(broken.clone()),
        ];
        let [swap_off, spread_off, failed_off, ttft_off] = &policies;
        let (swap, failed, ttft) = (
            turned_on(swap_off),
            turned_on(failed_off),
            turned_on(ttft_off),
        );
        assert!(store.replace_policy(swap_off, &swap).await?);
        assert!(store.replace_policy(spread_off, spread_off).await?);
        assert!(store.replace_policy(failed_off, &failed).await?);
        assert!(store.replace_policy(ttft_off, &ttft).await?);
        // Failed without codes, as an older binary stores it.
        let without_codes = |policy: CampaignGroupPolicy| CampaignGroupPolicy {
            settings: PolicySettings::default(),
            ..policy
        };
        let first_failed = PolicyKind::Failed.defaults(first);
        let uncoded_failed = PolicyKind::Failed.defaults(uncoded.clone());
        assert!(
            store
                .replace_policy(&first_failed, &without_codes(first_failed.clone()))
                .await?
        );
        assert!(
            store
                .replace_policy(&uncoded_failed, &without_codes(turned_on(&uncoded_failed)))
                .await?
        );
        // A row #1132 wrote, with no `max_reruns` or `settings`.
        sqlx::query(
            r#"INSERT INTO campaign_group_policies (group_id, kind, body)
               VALUES ($1, 'swap', '{"enabled": true}')"#,
        )
        .bind(broken.as_str())
        .execute(db.pool())
        .await?;

        let enabled = store.enabled_policies().await?;
        assert_eq!(enabled.policies, [swap, failed, ttft]);
        let reported: Vec<(&CampaignGroupId, &str)> = enabled
            .unreadable
            .iter()
            .map(|(group_id, report)| (group_id, report.key.as_str()))
            .collect();
        let keys = [
            format!("campaign_group_policies/{broken}/swap"),
            format!("campaign_group_policies/{uncoded}/failed"),
        ];
        assert_eq!(
            reported,
            [(&broken, keys[0].as_str()), (&uncoded, keys[1].as_str())]
        );
        assert_eq!(enabled.off, [idle]);
        Ok(())
    }

    /// A member's policy state as [`CampaignGroupStore::policy_states`] reads
    /// it, a report by its key.
    type StateRead = (CampaignId, Result<Option<GroupPolicyState>, String>);

    async fn states(
        store: &SqlCampaignGroupStore,
        group_id: &CampaignGroupId,
    ) -> anyhow::Result<Vec<StateRead>> {
        Ok(store
            .policy_states(group_id)
            .await?
            .into_iter()
            .map(|member| (member.campaign_id, member.policy_state.map_err(|e| e.key)))
            .collect())
    }

    /// A member's state reads `None` until written, then as written, and is
    /// reported by its row when it does not decode; its job states are written
    /// with it. A write for a campaign that moved to another group lands
    /// nowhere, and the move dropped the state and the job states.
    #[tokio::test]
    async fn a_policy_state_stays_with_its_membership() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let (group, other) = (a_stored_group(&store).await?, a_stored_group(&store).await?);
        let (a, b) = (a_campaign(db).await?, a_campaign(db).await?);
        store.add_members(&group, &[a.clone(), b.clone()]).await?;
        assert_eq!(
            states(&store, &group).await?,
            [(a.clone(), Ok(None)), (b.clone(), Ok(None))]
        );

        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let state = GroupPolicyState {
            policies: vec![turned_on(&PolicyKind::Spread.defaults(group.clone()))],
            triggered: BTreeSet::from([PolicyKind::Spread]),
            filed: Some(GroupFiling {
                plan_id: plan_id.clone(),
                queued: BTreeSet::from([JobId::try_new("job-2")?]),
                commit_unknown: true,
                filed_at: Timestamp::now(),
            }),
            refusal: Some(Refusal {
                plan_id,
                reason: "the deadline has passed".to_owned(),
                build: "2026.11.11-ea3ce9a".to_owned(),
            }),
            unreadable_at: Some(Timestamp::now()),
            rules: 1,
            deferred: Some(PolicyDeferral {
                until: Timestamp::now(),
                rerun_job_count: 3,
                marked_job_count: 1,
            }),
        };
        let (kept, flagged) = (
            a_job_state("job-1", &[])?,
            a_job_state("job-2", &[PolicyKind::Spread])?,
        );
        let both = JobStateChanges {
            put: vec![kept.clone(), flagged.clone()],
            removed: Vec::new(),
        };
        assert!(store.set_policy_state(&group, &a, &state, &both).await?);
        assert_eq!(
            store.job_states(&group, &a).await?,
            [kept.clone(), flagged.clone()]
        );
        let cleared = a_job_state("job-2", &[])?;
        let changes = JobStateChanges {
            put: vec![cleared.clone()],
            removed: vec![kept.job_id.clone()],
        };
        assert!(store.set_policy_state(&group, &a, &state, &changes).await?);
        assert_eq!(store.job_states(&group, &a).await?, [cleared]);
        sqlx::query(
            r#"UPDATE campaign_group_members SET policy_state = '{"triggered": 1}'
               WHERE campaign_id = $1"#,
        )
        .bind(b.as_str())
        .execute(db.pool())
        .await?;
        let unreadable: StateRead = (b.clone(), Err(format!("campaign_group_members/{b}")));
        assert_eq!(
            states(&store, &group).await?,
            [(a.clone(), Ok(Some(state.clone()))), unreadable.clone()]
        );
        // Read by campaign, whatever the group; a campaign in none is left out.
        let outside = a_campaign(db).await?;
        let read: Vec<StateRead> = store
            .member_states(&[outside, b.clone(), a.clone()])
            .await?
            .into_iter()
            .map(|member| (member.campaign_id, member.policy_state.map_err(|e| e.key)))
            .collect();
        assert_eq!(
            read,
            [(a.clone(), Ok(Some(state.clone()))), unreadable.clone()]
        );

        store.remove_member(&group, &a).await?;
        store.add_members(&other, std::slice::from_ref(&a)).await?;
        assert!(!store.set_policy_state(&group, &a, &state, &both).await?);
        assert_eq!(states(&store, &other).await?, [(a.clone(), Ok(None))]);
        assert_eq!(states(&store, &group).await?, [unreadable]);
        assert_eq!(store.job_states(&other, &a).await?, []);
        Ok(())
    }

    /// A clear writes the member's state and deletes its job states unread,
    /// one that does not decode included, and leaves a campaign of another
    /// group as it is.
    #[tokio::test]
    async fn a_clear_deletes_the_job_states_unread() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let (group, other) = (a_stored_group(&store).await?, a_stored_group(&store).await?);
        let (a, b) = (a_campaign(db).await?, a_campaign(db).await?);
        store.add_members(&group, std::slice::from_ref(&a)).await?;
        store.add_members(&other, std::slice::from_ref(&b)).await?;
        let state = GroupPolicyState {
            policies: vec![turned_on(&PolicyKind::Swap.defaults(group.clone()))],
            triggered: BTreeSet::from([PolicyKind::Swap]),
            ..GroupPolicyState::default()
        };
        let job_state = a_job_state("job-1", &[PolicyKind::Swap])?;
        let changes = JobStateChanges {
            put: vec![job_state.clone()],
            removed: Vec::new(),
        };
        assert!(store.set_policy_state(&group, &a, &state, &changes).await?);
        assert!(store.set_policy_state(&other, &b, &state, &changes).await?);
        sqlx::query(
            "UPDATE campaign_group_job_states SET body = 'not a job state' WHERE campaign_id = $1",
        )
        .bind(a.as_str())
        .execute(db.pool())
        .await?;
        let cleared = GroupPolicyState::default();
        assert!(store.clear_policy_state(&group, &a, &cleared).await?);
        assert_eq!(
            (
                states(&store, &group).await?,
                store.job_states(&group, &a).await?
            ),
            (vec![(a.clone(), Ok(Some(cleared.clone())))], Vec::new())
        );
        assert!(!store.clear_policy_state(&group, &b, &cleared).await?);
        assert_eq!(
            (
                states(&store, &other).await?,
                store.job_states(&other, &b).await?
            ),
            (vec![(b.clone(), Ok(Some(state)))], vec![job_state])
        );
        Ok(())
    }

    /// A swap re-run of `job` in `campaign_id`, filed at `filed_at` over a
    /// run that swapped `before` bytes, waiting.
    fn a_rerun(
        campaign_id: &CampaignId,
        job: &str,
        filed_at: Timestamp,
        before: f64,
    ) -> anyhow::Result<PolicyRerun> {
        Ok(PolicyRerun {
            campaign_id: campaign_id.clone(),
            kind: PolicyKind::Swap,
            rerun_job_id: JobId::try_new(job)?,
            rerun_of: JobId::try_new(format!("{job}-before"))?,
            plan_id: PlanId::from_uuid(uuid::Uuid::now_v7()),
            filed_at,
            attempt: 1,
            benchmark_id: "decode_throughput_512_100".to_owned(),
            model_key: Some("lfm2-700m".to_owned()),
            before,
            threshold: Some(0.0),
            short_repetition_ms: None,
            short_repetition_percent: None,
            codes: None,
            before_code: None,
            after: None,
            after_code: None,
            outcome: None,
            judged_at: None,
        })
    }

    fn judged(
        rerun: &PolicyRerun,
        outcome: RerunOutcome,
        after: Option<f64>,
    ) -> anyhow::Result<PolicyRerun> {
        Ok(PolicyRerun {
            after,
            outcome: Some(outcome),
            judged_at: Some(
                Timestamp::now().round(
                    jiff::TimestampRound::new()
                        .smallest(jiff::Unit::Microsecond)
                        .mode(jiff::RoundMode::Trunc),
                )?,
            ),
            ..rerun.clone()
        })
    }

    /// A re-run waits until its outcome is written once: recording it again,
    /// or judging it again, writes nothing. Another group's row of the same
    /// job is its own.
    #[tokio::test]
    async fn a_rerun_waits_until_its_outcome_is_written_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let (group, other) = (a_stored_group(&store).await?, a_stored_group(&store).await?);
        let (a, b) = (a_campaign(db).await?, a_campaign(db).await?);
        let at = Timestamp::now().round(
            jiff::TimestampRound::new()
                .smallest(jiff::Unit::Microsecond)
                .mode(jiff::RoundMode::Trunc),
        )?;
        let swapped = a_rerun(&a, "job-1", at, 4096.0)?;
        let spread = PolicyRerun {
            kind: PolicyKind::Spread,
            before: 10.0,
            threshold: Some(5.0),
            short_repetition_ms: Some(1000.0),
            short_repetition_percent: Some(10.0),
            ..swapped.clone()
        };
        let failed = PolicyRerun {
            kind: PolicyKind::Failed,
            before: 1.0,
            threshold: None,
            codes: Some(vec![FailureCode::HarnessError, FailureCode::DeviceError]),
            before_code: Some("harness_error".to_owned()),
            ..swapped.clone()
        };
        let theirs = a_rerun(&b, "job-2", at, 1.0)?;
        let recorded = [swapped.clone(), spread.clone(), failed.clone(), theirs];
        assert_eq!(store.record_reruns(&group, &recorded).await?, 4);
        assert_eq!(
            store
                .record_reruns(&other, std::slice::from_ref(&swapped))
                .await?,
            1
        );
        let again = PolicyRerun {
            attempt: 2,
            ..swapped.clone()
        };
        assert_eq!(store.record_reruns(&group, &[again]).await?, 0);
        assert_eq!(
            store.waiting_reruns(&group, &a).await?,
            [failed.clone(), spread.clone(), swapped.clone()]
        );

        let cleared = judged(&swapped, RerunOutcome::Cleared, Some(0.0))?;
        let flagged = judged(&swapped, RerunOutcome::Flagged, Some(4096.0))?;
        assert_eq!(
            store
                .judge_reruns(&group, std::slice::from_ref(&cleared))
                .await?,
            1
        );
        assert_eq!(store.judge_reruns(&group, &[flagged]).await?, 0);
        assert_eq!(store.waiting_reruns(&group, &a).await?, [failed, spread]);
        let (_, rows) = store
            .rerun_results(&group, std::slice::from_ref(&a), 0, None)
            .await?;
        assert!(rows.contains(&cleared), "{rows:?}");
        assert_eq!(store.waiting_reruns(&other, &a).await?, [swapped]);
        Ok(())
    }

    /// The rows of the campaigns asked for come newest filing first, a page
    /// at a time, with each kind's count per outcome and the medians of its
    /// rows read, which leave out the waiting and dropped ones, over all of
    /// them.
    #[tokio::test]
    async fn rerun_results_sum_up_each_kind_and_page_the_rows() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlCampaignGroupStore::new(db.clone());
        let group = a_stored_group(&store).await?;
        let (a, b) = (a_campaign(db).await?, a_campaign(db).await?);
        let at = Timestamp::now().round(
            jiff::TimestampRound::new()
                .smallest(jiff::Unit::Microsecond)
                .mode(jiff::RoundMode::Trunc),
        )?;
        let minutes = SignedDuration::from_mins;
        let failed = a_rerun(&a, "job-6", at - minutes(5), 3072.0)?;
        let dropped = a_rerun(&a, "job-7", at - minutes(4), 8192.0)?;
        let waiting = a_rerun(&a, "job-1", at - minutes(3), 4096.0)?;
        let cleared = a_rerun(&a, "job-2", at - minutes(2), 1024.0)?;
        let flagged = a_rerun(&a, "job-3", at - minutes(1), 2048.0)?;
        let spread = PolicyRerun {
            kind: PolicyKind::Spread,
            threshold: Some(5.0),
            short_repetition_ms: None,
            short_repetition_percent: None,
            ..a_rerun(&a, "job-4", at, 10.0)?
        };
        let theirs = a_rerun(&b, "job-5", at, 1.0)?;
        let recorded = [
            failed.clone(),
            dropped.clone(),
            waiting,
            cleared.clone(),
            flagged.clone(),
            spread.clone(),
            theirs,
        ];
        assert_eq!(store.record_reruns(&group, &recorded).await?, 7);
        let outcomes = [
            judged(&failed, RerunOutcome::Failed, None)?,
            judged(&dropped, RerunOutcome::Dropped, None)?,
            judged(&cleared, RerunOutcome::Cleared, Some(0.0))?,
            judged(&flagged, RerunOutcome::Flagged, Some(512.0))?,
            judged(&spread, RerunOutcome::Cleared, Some(2.0))?,
        ];
        assert_eq!(store.judge_reruns(&group, &outcomes).await?, 5);

        let (summary, page) = store
            .rerun_results(&group, std::slice::from_ref(&a), 1, Some(2))
            .await?;
        assert_eq!(
            summary,
            [
                PolicyRerunSummary {
                    kind: PolicyKind::Swap,
                    reruns: 5,
                    waiting: 1,
                    cleared: 1,
                    flagged: 1,
                    failed: 1,
                    dropped: 1,
                    median_before: Some(2048.0),
                    median_after: Some(256.0),
                },
                PolicyRerunSummary {
                    kind: PolicyKind::Spread,
                    reruns: 1,
                    waiting: 0,
                    cleared: 1,
                    flagged: 0,
                    failed: 0,
                    dropped: 0,
                    median_before: Some(10.0),
                    median_after: Some(2.0),
                },
            ]
        );
        let jobs = |rows: &[PolicyRerun]| -> Vec<String> {
            rows.iter()
                .map(|rerun| rerun.rerun_job_id.to_string())
                .collect()
        };
        assert_eq!(jobs(&page), ["job-3", "job-2"]);
        let (_, all) = store.rerun_results(&group, &[a, b], 0, None).await?;
        assert_eq!(
            jobs(&all),
            [
                "job-4", "job-5", "job-3", "job-2", "job-1", "job-7", "job-6"
            ]
        );
        let (none, empty) = store.rerun_results(&group, &[], 0, None).await?;
        assert!(none.is_empty() && empty.is_empty());
        Ok(())
    }
}
