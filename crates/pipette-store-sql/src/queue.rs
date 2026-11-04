//! PostgreSQL storage for queue jobs, leases, markers, and maintenance state.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};

use pipette_core::denial::DenialRecord;
use pipette_core::todo_filename::{avail_filename, leased_key, tmp_filename};
use pipette_plan_types::{ClientId, ExpiresAt, JobId};
use pipette_store::{
    BacklogSnapshot, CampaignBacklogSnapshot, ClaimResult, LivenessImport, RecycleResult,
    RedateResult, RenewLeaseResult, SuspensionRecord, TodoStore,
};

use crate::{SqlDatabase, plans::first_revision};

/// Queue storage on PostgreSQL.
#[derive(Clone, Debug)]
pub struct SqlTodoStore {
    db: SqlDatabase,
}

impl SqlTodoStore {
    pub fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
    async fn read_state(&self, name: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("read queue state", || {
                sqlx::query_as("SELECT body FROM queue_maintenance_state WHERE name = $1")
                    .bind(name)
            })
            .await?;
        Ok(row.map(|(body,)| body))
    }

    async fn write_state(&self, name: &str, body: &str) -> anyhow::Result<()> {
        self.db
            .execute("write queue state", || {
                sqlx::query(
                    "INSERT INTO queue_maintenance_state (name, body) VALUES ($1, $2) ON CONFLICT \
                     (name) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(name)
                .bind(body)
            })
            .await?;
        Ok(())
    }

    /// A missing or corrupt body reads as empty ([`pipette_store::parse_key_set`]).
    async fn read_key_set<T>(&self, name: &str) -> anyhow::Result<HashSet<T>>
    where
        T: serde::de::DeserializeOwned + Eq + std::hash::Hash,
    {
        Ok(self
            .read_state(name)
            .await?
            .map(|body| pipette_store::parse_key_set(body.as_bytes(), name))
            .unwrap_or_default())
    }

    async fn write_key_set<T: serde::Serialize + Sync>(
        &self,
        name: &str,
        set: &HashSet<T>,
    ) -> anyhow::Result<()> {
        self.write_state(name, &serde_json::to_string(set)?).await
    }
}

const ELIGIBLE_INDEXED: &str = "eligible-indexed";
const ELIGIBLE_WORKERS: &str = "eligible-workers";
const GC_CANDIDATES: &str = "gc-candidates";
const BACKLOG: &str = "backlog";
const BACKLOG_CAMPAIGNS: &str = "backlog-campaigns";

fn valid_client(raw: &str) -> Option<ClientId> {
    ClientId::try_new(raw)
        .inspect_err(|error| {
            tracing::warn!(client_id = raw, %error, "skipping queue row with an invalid id");
        })
        .ok()
}

fn to_second(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(0)
}

fn expiry(at: ExpiresAt) -> Option<DateTime<Utc>> {
    at.instant().map(to_second)
}

fn deadline(at: Option<DateTime<Utc>>) -> ExpiresAt {
    at.map_or(ExpiresAt::Never, ExpiresAt::At)
}

fn parse_body(job: &JobId, body: &str) -> anyhow::Result<serde_json::Value> {
    serde_json::from_str(body).with_context(|| format!("invalid queue body for job {job}"))
}

fn valid_job(raw: &str) -> Option<JobId> {
    JobId::try_new(raw)
        .inspect_err(|error| {
            tracing::warn!(job_id = raw, %error, "skipping queue row with an invalid id");
        })
        .ok()
}

#[derive(sqlx::FromRow)]
struct LeaseRow {
    job_id: String,
    expires_at: Option<DateTime<Utc>>,
    leased_by: String,
    lease_expiry: DateTime<Utc>,
}

impl LeaseRow {
    fn key(self) -> Option<String> {
        let job = valid_job(&self.job_id)?;
        let client = ClientId::try_new(&self.leased_by)
            .inspect_err(|error| {
                tracing::warn!(
                    job_id = self.job_id,
                    client_id = self.leased_by,
                    %error,
                    "skipping queue row with an invalid id"
                );
            })
            .ok()?;
        Some(leased_key(
            &job,
            deadline(self.expires_at),
            &client,
            self.lease_expiry,
        ))
    }
}

#[async_trait]
impl TodoStore for SqlTodoStore {
    async fn list_avail(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("list available jobs", || {
                sqlx::query_as("SELECT job_id, expires_at FROM queue_jobs WHERE leased_by IS NULL")
            })
            .await?;
        let mut keys: Vec<_> = rows
            .into_iter()
            .filter_map(|(job, exp)| valid_job(&job).map(|job| avail_filename(&job, deadline(exp))))
            .collect();
        keys.sort_unstable();
        Ok(keys)
    }

    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("get_avail", || {
                sqlx::query_as(
                    "SELECT body FROM queue_jobs WHERE job_id = $1 AND leased_by IS \
                     NULL AND expires_at IS NOT DISTINCT FROM $2",
                )
                .bind(job_id.to_string())
                .bind(expiry(expires_at))
            })
            .await?;
        row.map(|(body,)| parse_body(job_id, &body)).transpose()
    }

    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        self.db
            .execute("delete_avail", || {
                sqlx::query(
                    "DELETE FROM queue_jobs WHERE job_id = $1 AND leased_by IS NULL \
                     AND expires_at IS NOT DISTINCT FROM $2",
                )
                .bind(job_id.to_string())
                .bind(expiry(expires_at))
            })
            .await?;
        Ok(())
    }

    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.db
            .execute("delete_avail_by_job", || {
                sqlx::query("DELETE FROM queue_jobs WHERE job_id = $1 AND leased_by IS NULL")
                    .bind(job_id.to_string())
            })
            .await?;
        Ok(())
    }

    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("get_avail_by_job", || {
                sqlx::query_as(
                    "SELECT body FROM queue_jobs WHERE job_id = $1 AND leased_by IS \
                     NULL",
                )
                .bind(job_id.to_string())
            })
            .await?;
        row.map(|(body,)| parse_body(job_id, &body)).transpose()
    }

    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<LeaseRow> = self
            .db
            .fetch_all("list_leased", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at, leased_by, lease_expiry FROM \
                     queue_jobs WHERE leased_by IS NOT NULL",
                )
            })
            .await?;
        Ok(rows.into_iter().filter_map(LeaseRow::key).collect())
    }

    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        let rows: Vec<LeaseRow> = self
            .db
            .fetch_all("list_leased_for_client", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at, leased_by, lease_expiry FROM \
                     queue_jobs WHERE leased_by = $1",
                )
                .bind(client_id.as_str())
            })
            .await?;
        Ok(rows.into_iter().filter_map(LeaseRow::key).collect())
    }

    async fn get_leased(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("get_leased", || {
                sqlx::query_as(
                    "SELECT body FROM queue_jobs WHERE job_id = $1 AND expires_at IS \
                     NOT DISTINCT FROM $2 AND leased_by = $3 AND lease_expiry = $4",
                )
                .bind(job_id.to_string())
                .bind(expiry(expires_at))
                .bind(client_id.to_string())
                .bind(to_second(lease_expiry))
            })
            .await?;
        row.map(|(body,)| parse_body(job_id, &body)).transpose()
    }

    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult> {
        // The outer read sees the pre-statement holder, including on a zero-row update.
        let (changed, exp, holder): (bool, Option<DateTime<Utc>>, Option<String>) = self
            .db
            .fetch_one("renew queue lease", || {
                sqlx::query_as(
                    "WITH renewed AS ( UPDATE queue_jobs SET lease_expiry = $3, \
                     revision = revision + 1 WHERE job_id = $1 AND leased_by = $2 \
                     RETURNING expires_at) SELECT EXISTS (SELECT 1 FROM renewed), \
                     (SELECT expires_at FROM renewed), (SELECT leased_by FROM \
                     queue_jobs WHERE job_id = $1)",
                )
                .bind(job_id.as_str())
                .bind(client_id.as_str())
                .bind(to_second(new_expiry))
            })
            .await?;
        Ok(if changed {
            RenewLeaseResult::Renewed(deadline(exp))
        } else if holder.is_some() {
            RenewLeaseResult::WrongClient
        } else {
            RenewLeaseResult::NotFound
        })
    }

    async fn delete_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.db
            .execute("delete_lease", || {
                sqlx::query(
                    "DELETE FROM queue_jobs WHERE job_id = $1 AND expires_at IS NOT \
                     DISTINCT FROM $2 AND leased_by = $3 AND lease_expiry = $4",
                )
                .bind(job_id.to_string())
                .bind(expiry(expires_at))
                .bind(client_id.to_string())
                .bind(to_second(lease_expiry))
            })
            .await?;
        Ok(())
    }

    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        // A repeat of a committed claim by the same client and lease succeeds, as the
        // S3 rename does.
        let row: Option<(String,)> = self
            .db
            .fetch_optional("claim queue job", || {
                sqlx::query_as(
                    "UPDATE queue_jobs SET leased_by = $3, lease_expiry = $4, revision \
                     = revision + 1 WHERE job_id = $1 AND expires_at IS NOT DISTINCT \
                     FROM $2 AND (leased_by IS NULL OR (leased_by = $3 AND \
                     lease_expiry = $4)) RETURNING body",
                )
                .bind(job_id.to_string())
                .bind(expiry(expires_at))
                .bind(client_id.to_string())
                .bind(to_second(lease_expiry))
            })
            .await?;
        match row {
            Some((body,)) => Ok(ClaimResult::Claimed(parse_body(job_id, &body)?)),
            None => Ok(ClaimResult::Gone),
        }
    }

    async fn avail_key_for_job(&self, job_id: &JobId) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<DateTime<Utc>>,)> = self
            .db
            .fetch_optional("avail key", || {
                sqlx::query_as(
                    "SELECT expires_at FROM queue_jobs WHERE job_id = $1 AND leased_by \
                     IS NULL",
                )
                .bind(job_id.to_string())
            })
            .await?;
        Ok(row.map(|(exp,)| avail_filename(job_id, deadline(exp))))
    }

    async fn redate_avail(
        &self,
        job_id: &JobId,
        from: ExpiresAt,
        to: ExpiresAt,
    ) -> anyhow::Result<RedateResult> {
        let rows = self
            .db
            .execute("redate_avail", || {
                sqlx::query(
                    "UPDATE queue_jobs SET expires_at = $3, revision = revision + 1 \
                     WHERE job_id = $1 AND leased_by IS NULL AND expires_at IS NOT \
                     DISTINCT FROM $2",
                )
                .bind(job_id.to_string())
                .bind(expiry(from))
                .bind(expiry(to))
            })
            .await?;
        Ok(if rows == 0 {
            RedateResult::Gone
        } else {
            RedateResult::Redated
        })
    }

    async fn recycle_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        // A lost recycle race succeeds only when the exact available key exists.
        let (changed, available): (bool, bool) = self
            .db
            .fetch_one("recycle queue lease", || {
                sqlx::query_as(
                    "WITH recycled AS (UPDATE queue_jobs SET leased_by = NULL, \
                     lease_expiry = NULL, revision = revision + 1 WHERE job_id = $1 \
                     AND expires_at IS NOT DISTINCT FROM $2 AND leased_by = $3 AND \
                     lease_expiry = $4 RETURNING 1) SELECT EXISTS (SELECT 1 FROM \
                     recycled), EXISTS (SELECT 1 FROM queue_jobs WHERE job_id = $1 AND \
                     leased_by IS NULL AND expires_at IS NOT DISTINCT FROM $2)",
                )
                .bind(job_id.to_string())
                .bind(expiry(expires_at))
                .bind(client_id.to_string())
                .bind(to_second(lease_expiry))
            })
            .await?;
        Ok(if changed || available {
            RecycleResult::Recycled
        } else {
            RecycleResult::Gone
        })
    }

    async fn write_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        record: &DenialRecord,
    ) -> anyhow::Result<()> {
        self.db
            .execute("write_denied", || {
                sqlx::query(
                    "INSERT INTO queue_denials (job_id, client_id, denied_at, failure_reason, \
                     client_version, runtime_version) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT \
                     (job_id, client_id) DO UPDATE SET denied_at = EXCLUDED.denied_at, \
                     failure_reason = EXCLUDED.failure_reason, client_version = \
                     EXCLUDED.client_version, runtime_version = EXCLUDED.runtime_version",
                )
                .bind(job_id.as_str())
                .bind(client_id.as_str())
                .bind(record.denied_at)
                .bind(record.failure_reason.as_deref())
                .bind(record.client_version.as_deref())
                .bind(record.runtime_version.as_deref())
            })
            .await?;
        Ok(())
    }

    async fn get_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<DenialRecord>> {
        let row: Option<(
            Option<DateTime<Utc>>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = self
            .db
            .fetch_optional("get_denied", || {
                sqlx::query_as(
                    "SELECT denied_at, failure_reason, client_version, runtime_version FROM \
                     queue_denials WHERE job_id = $1 AND client_id = $2",
                )
                .bind(job_id.as_str())
                .bind(client_id.as_str())
            })
            .await?;
        Ok(row.map(
            |(denied_at, failure_reason, client_version, runtime_version)| DenialRecord {
                denied_at,
                failure_reason,
                client_version,
                runtime_version,
            },
        ))
    }

    async fn is_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<bool> {
        let (exists,): (bool,) = self.db.fetch_one("is_denied", || { sqlx::query_as("SELECT EXISTS (SELECT 1 FROM queue_denials WHERE job_id = $1 AND client_id = $2)").bind(job_id.as_str()).bind(client_id.as_str()) }).await?;
        Ok(exists)
    }

    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.db
            .execute("delete_denied_for_job", || {
                sqlx::query("DELETE FROM queue_denials WHERE job_id = $1").bind(job_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_denied", || {
                sqlx::query("DELETE FROM queue_denials WHERE job_id = $1 AND client_id = $2")
                    .bind(job_id.as_str())
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_all_denied", || {
                sqlx::query_as(
                    "SELECT job_id, client_id FROM queue_denials ORDER BY job_id, client_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(j, c)| Some((valid_job(&j)?, valid_client(&c)?)))
            .collect())
    }

    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
        let rows: Vec<(String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("list_eligible_for_client", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at FROM queue_eligibility WHERE client_id = $1 \
                     ORDER BY job_id",
                )
                .bind(client_id.as_str())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(j, e)| Some((valid_job(&j)?, deadline(e))))
            .collect())
    }

    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.db
            .execute("write_eligible", || {
                sqlx::query(
                    "INSERT INTO queue_eligibility (client_id, job_id, expires_at) VALUES ($1, $2, \
                     $3) ON CONFLICT (client_id, job_id) DO UPDATE SET expires_at = \
                     EXCLUDED.expires_at",
                )
                .bind(client_id.as_str())
                .bind(job_id.as_str())
                .bind(expiry(expires_at))
            })
            .await?;
        Ok(())
    }

    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_eligible_for_client", || {
                sqlx::query("DELETE FROM queue_eligibility WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.db
            .execute("delete_eligible", || {
                sqlx::query(
                    "DELETE FROM queue_eligibility WHERE client_id = $1 AND job_id = $2 AND \
                     expires_at IS NOT DISTINCT FROM $3",
                )
                .bind(client_id.as_str())
                .bind(job_id.as_str())
                .bind(expiry(expires_at))
            })
            .await?;
        Ok(())
    }

    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        let rows: Vec<(String, String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("list_all_eligible", || {
                sqlx::query_as(
                    "SELECT client_id, job_id, expires_at FROM queue_eligibility ORDER BY \
                     client_id, job_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, j, e)| Some((valid_client(&c)?, valid_job(&j)?, deadline(e))))
            .collect())
    }

    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let request_id = uuid::Uuid::now_v7().to_string();
        self.db
            .execute("write_pending_reindex", || {
                sqlx::query(
                    "INSERT INTO queue_reindex_requests (request_id, client_id, \
                     requested_at) VALUES ($1, $2, now())",
                )
                .bind(request_id.clone())
                .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_pending_reindex", || {
                sqlx::query_as(
                    "SELECT client_id, request_id FROM queue_reindex_requests ORDER BY \
                     client_id, request_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, r)| Some((valid_client(&c)?, format!("{c}.{r}"))))
            .collect())
    }

    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()> {
        let Some((_, request_id)) = key.rsplit_once('.') else {
            return Ok(());
        };
        self.db
            .execute("delete_pending_reindex", || {
                sqlx::query("DELETE FROM queue_reindex_requests WHERE request_id = $1")
                    .bind(request_id)
            })
            .await?;
        Ok(())
    }

    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        let (exists,): (bool,) = self
            .db
            .fetch_one("has_pending_reindex", || {
                sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM queue_reindex_requests WHERE client_id = $1)",
                )
                .bind(client_id.as_str())
            })
            .await?;
        Ok(exists)
    }

    async fn write_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.db
            .execute("write_pending_reindex_job", || {
                sqlx::query(
                    "INSERT INTO queue_reindex_jobs (job_id) VALUES ($1) ON CONFLICT DO NOTHING",
                )
                .bind(job_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_pending_reindex_jobs", || {
                sqlx::query_as("SELECT job_id FROM queue_reindex_jobs ORDER BY job_id")
            })
            .await?;
        Ok(rows.into_iter().filter_map(|(j,)| valid_job(&j)).collect())
    }

    async fn delete_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.db
            .execute("delete_pending_reindex_job", || {
                sqlx::query("DELETE FROM queue_reindex_jobs WHERE job_id = $1")
                    .bind(job_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn write_lost_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.db
            .execute("write_lost_lease", || {
                sqlx::query(
                    "INSERT INTO queue_lost_leases (job_id, client_id, lease_expiry) VALUES ($1, \
                     $2, $3) ON CONFLICT DO NOTHING",
                )
                .bind(job_id.as_str())
                .bind(client_id.as_str())
                .bind(to_second(lease_expiry))
            })
            .await?;
        Ok(())
    }

    async fn list_lost_leases(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        let rows: Vec<(String, DateTime<Utc>)> = self
            .db
            .fetch_all("list_lost_leases", || {
                sqlx::query_as(
                    "SELECT client_id, lease_expiry FROM queue_lost_leases WHERE job_id = $1 ORDER \
                     BY lease_expiry, client_id",
                )
                .bind(job_id.as_str())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, e)| Some((valid_client(&c)?, e)))
            .collect())
    }

    async fn delete_lost_leases_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.db
            .execute("delete_lost_leases_for_job", || {
                sqlx::query("DELETE FROM queue_lost_leases WHERE job_id = $1").bind(job_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_lost_lease_jobs", || {
                sqlx::query_as("SELECT DISTINCT job_id FROM queue_lost_leases ORDER BY job_id")
            })
            .await?;
        Ok(rows.into_iter().filter_map(|(j,)| valid_job(&j)).collect())
    }

    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let body = serde_json::to_string(body)?;
        self.db
            .execute("write_tmp", || {
                sqlx::query(
                    "INSERT INTO queue_tmp (job_id, body) VALUES ($1, $2) ON CONFLICT \
                     (job_id) DO UPDATE SET body = EXCLUDED.body, written_at = now()",
                )
                .bind(job_id.to_string())
                .bind(body.clone())
            })
            .await?;
        Ok(())
    }

    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        let revision = first_revision();
        let (moved, inserted, present): (bool, bool, bool) = self
            .db
            .fetch_one("promote queue job", || {
                sqlx::query_as(
                    "WITH moved AS (DELETE FROM queue_tmp WHERE job_id = $1 RETURNING \
                     body), ins AS (INSERT INTO queue_jobs (job_id, body, expires_at, \
                     revision) SELECT $1, body, $2, $3 FROM moved ON CONFLICT DO \
                     NOTHING RETURNING 1) SELECT EXISTS (SELECT 1 FROM moved), EXISTS \
                     (SELECT 1 FROM ins), EXISTS (SELECT 1 FROM queue_jobs WHERE job_id \
                     = $1 AND expires_at IS NOT DISTINCT FROM $2)",
                )
                .bind(job_id.as_str())
                .bind(expiry(expires_at))
                .bind(revision)
            })
            .await?;
        anyhow::ensure!(
            moved || present,
            "no staged body, and no queued body at that deadline, for job {job_id}"
        );
        if moved && !inserted {
            tracing::warn!(%job_id, "staged job body dropped: the job is already queued");
        }
        Ok(())
    }

    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list stale queue staging", || {
                sqlx::query_as(
                    "SELECT job_id FROM queue_tmp WHERE written_at <= now() - $1 * \
                     interval '1 second'",
                )
                .bind(age.as_secs_f64())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(job,)| valid_job(&job).map(|job| tmp_filename(&job)))
            .collect())
    }

    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()> {
        let Some(job) = key.strip_suffix(".json") else {
            return Ok(());
        };
        self.db
            .execute("delete_tmp_object", || {
                sqlx::query("DELETE FROM queue_tmp WHERE job_id = $1").bind(job.to_owned())
            })
            .await?;
        Ok(())
    }

    async fn read_eligible_indexed(&self) -> anyhow::Result<HashSet<String>> {
        self.read_key_set(ELIGIBLE_INDEXED).await
    }

    async fn write_eligible_indexed(&self, indexed: &HashSet<String>) -> anyhow::Result<()> {
        self.write_key_set(ELIGIBLE_INDEXED, indexed).await
    }

    async fn read_eligible_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
        self.read_key_set(ELIGIBLE_WORKERS).await
    }

    async fn write_eligible_workers(&self, workers: &HashSet<ClientId>) -> anyhow::Result<()> {
        self.write_key_set(ELIGIBLE_WORKERS, workers).await
    }

    async fn read_backlog(&self) -> anyhow::Result<Option<BacklogSnapshot>> {
        Ok(self
            .read_state(BACKLOG)
            .await?
            .map(|body| pipette_store::parse_snapshot(body.as_bytes(), BACKLOG))
            .unwrap_or_default())
    }

    async fn write_backlog(&self, snapshot: &BacklogSnapshot) -> anyhow::Result<()> {
        self.write_state(BACKLOG, &serde_json::to_string(snapshot)?)
            .await
    }

    async fn read_campaign_backlog(&self) -> anyhow::Result<Option<CampaignBacklogSnapshot>> {
        Ok(self
            .read_state(BACKLOG_CAMPAIGNS)
            .await?
            .map(|body| pipette_store::parse_snapshot(body.as_bytes(), BACKLOG_CAMPAIGNS))
            .unwrap_or_default())
    }

    async fn write_campaign_backlog(
        &self,
        snapshot: &CampaignBacklogSnapshot,
    ) -> anyhow::Result<()> {
        self.write_state(BACKLOG_CAMPAIGNS, &serde_json::to_string(snapshot)?)
            .await
    }

    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        self.read_key_set(GC_CANDIDATES).await
    }

    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
        self.write_key_set(GC_CANDIDATES, candidates).await
    }

    async fn write_suspension(
        &self,
        client_id: &ClientId,
        suspended_at: DateTime<Utc>,
        conflicting_job_id: &JobId,
    ) -> anyhow::Result<()> {
        self.db
            .execute("write_suspension", || {
                sqlx::query(
                    "INSERT INTO queue_suspensions (client_id, suspended_at, conflicting_job_id) \
                     VALUES ($1, $2, $3) ON CONFLICT (client_id) DO UPDATE SET suspended_at = \
                     EXCLUDED.suspended_at, conflicting_job_id = EXCLUDED.conflicting_job_id",
                )
                .bind(client_id.as_str())
                .bind(suspended_at)
                .bind(conflicting_job_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn read_suspension(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<SuspensionRecord>> {
        let row: Option<(DateTime<Utc>, String)> = self
            .db
            .fetch_optional("read_suspension", || {
                sqlx::query_as(
                    "SELECT suspended_at, conflicting_job_id FROM queue_suspensions WHERE \
                     client_id = $1",
                )
                .bind(client_id.as_str())
            })
            .await?;
        row.map(|(suspended_at, j)| {
            Ok(SuspensionRecord {
                suspended_at,
                conflicting_job_id: JobId::try_new(j)?,
            })
        })
        .transpose()
    }

    async fn delete_suspension(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_suspension", || {
                sqlx::query("DELETE FROM queue_suspensions WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        let rows: Vec<(String, DateTime<Utc>, String)> = self
            .db
            .fetch_all("list_suspensions", || {
                sqlx::query_as(
                    "SELECT client_id, suspended_at, conflicting_job_id FROM \
                     queue_suspensions ORDER BY client_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, suspended_at, j)| {
                Some((
                    valid_client(&c)?,
                    SuspensionRecord {
                        suspended_at,
                        conflicting_job_id: valid_job(&j)?,
                    },
                ))
            })
            .collect())
    }

    async fn list_suspended_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_suspended_ids", || {
                sqlx::query_as("SELECT client_id FROM queue_suspensions ORDER BY client_id")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c,)| valid_client(&c))
            .collect())
    }

    async fn stamp_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("stamp_liveness", || {
                sqlx::query(
                    "INSERT INTO client_liveness (client_id, last_seen_at) VALUES ($1, now()) ON \
                     CONFLICT (client_id) DO UPDATE SET last_seen_at = now()",
                )
                .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn read_liveness(&self, client_id: &ClientId) -> anyhow::Result<Option<DateTime<Utc>>> {
        let row: Option<(DateTime<Utc>,)> = self
            .db
            .fetch_optional("read_liveness", || {
                sqlx::query_as("SELECT last_seen_at FROM client_liveness WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(row.map(|(at,)| at))
    }

    async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        let rows: Vec<(String, DateTime<Utc>)> = self
            .db
            .fetch_all("list_liveness", || {
                sqlx::query_as(
                    "SELECT client_id, last_seen_at FROM client_liveness ORDER BY client_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, e)| Some((valid_client(&c)?, e)))
            .collect())
    }

    async fn delete_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_liveness", || {
                sqlx::query("DELETE FROM client_liveness WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn validate_backend(&self) -> anyhow::Result<()> {
        crate::check(&self.db).await
    }
}

/// The instant is stored to the second, the precision of an S3 modification time.
#[async_trait]
impl LivenessImport for SqlTodoStore {
    async fn set_liveness(
        &self,
        client_id: &ClientId,
        last_seen_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.db
            .execute("set_liveness", || {
                sqlx::query(
                    "INSERT INTO client_liveness (client_id, last_seen_at) VALUES ($1, $2) ON \
                     CONFLICT (client_id) DO UPDATE SET last_seen_at = EXCLUDED.last_seen_at",
                )
                .bind(client_id.as_str())
                .bind(to_second(last_seen_at))
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::testing::{RACERS, race, test_db_or_skip};

    fn now() -> anyhow::Result<DateTime<Utc>> {
        Ok("2026-11-01T08:30:00.250Z".parse()?)
    }

    async fn enqueue(store: &SqlTodoStore, job: &JobId) -> anyhow::Result<()> {
        store
            .write_tmp(job, &serde_json::json!({"job": job}))
            .await?;
        store.promote_avail(job, ExpiresAt::Never).await
    }

    #[rstest]
    #[case(ELIGIBLE_INDEXED)]
    #[case(ELIGIBLE_WORKERS)]
    #[case(GC_CANDIDATES)]
    #[case(BACKLOG)]
    #[case(BACKLOG_CAMPAIGNS)]
    #[tokio::test]
    async fn a_corrupt_state_blob_reads_as_empty(#[case] name: &str) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        sqlx::query("INSERT INTO queue_maintenance_state (name, body) VALUES ($1, 'junk')")
            .bind(name)
            .execute(test_db.db().pool())
            .await?;
        assert!(store.read_eligible_indexed().await?.is_empty());
        assert!(store.read_eligible_workers().await?.is_empty());
        assert!(store.read_gc_candidates().await?.is_empty());
        assert!(store.read_backlog().await?.is_none());
        assert!(store.read_campaign_backlog().await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_redated_marker_replaces_the_old_deadline() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("redated_client")?;
        let job = JobId::try_new("redated-job")?;
        let old = ExpiresAt::At(now()?);
        store.write_eligible(&client, &job, old).await?;
        store
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        assert_eq!(
            store.list_all_eligible().await?,
            vec![(client.clone(), job.clone(), ExpiresAt::Never)]
        );
        store.delete_eligible(&client, &job, old).await?;
        assert_eq!(
            store.list_all_eligible().await?,
            vec![(client, job, ExpiresAt::Never)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn set_liveness_keeps_the_given_instant_to_the_second() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("liveness_client")?;
        store.stamp_liveness(&client).await?;
        store.set_liveness(&client, now()?).await?;
        assert_eq!(store.read_liveness(&client).await?, Some(to_second(now()?)));
        Ok(())
    }

    #[tokio::test]
    async fn sql_todo_store_passes_the_todo_contract() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        store.validate_backend().await?;
        pipette_store_testkit::assert_todo_store_contract(&store, now()?).await
    }

    #[tokio::test]
    async fn racing_claims_lease_the_job_once() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("race-job")?;
        enqueue(&store, &job).await?;
        let expiry = now()?;
        let results = race(|n| {
            let store = store.clone();
            let job = job.clone();
            async move {
                let client = ClientId::try_new(format!("race_{n}"))?;
                store
                    .claim_job(&job, ExpiresAt::Never, &client, expiry)
                    .await
            }
        })
        .await?;
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, ClaimResult::Claimed(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, ClaimResult::Gone))
                .count(),
            RACERS - 1
        );
        assert_eq!(store.list_leased().await?.len(), 1);
        assert!(store.list_avail().await?.is_empty());
        Ok(())
    }

    #[derive(Clone, Copy, Debug)]
    enum AvailCall {
        Delete,
        DeleteByJob,
        Redate,
    }

    #[rstest]
    #[case(AvailCall::Delete)]
    #[case(AvailCall::DeleteByJob)]
    #[case(AvailCall::Redate)]
    #[tokio::test]
    async fn a_leased_job_is_out_of_reach_of_the_avail_calls(
        #[case] call: AvailCall,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("leased-job")?;
        let client = ClientId::try_new("lease_client")?;
        let at = now()?;
        enqueue(&store, &job).await?;
        assert!(matches!(
            store.claim_job(&job, ExpiresAt::Never, &client, at).await?,
            ClaimResult::Claimed(_)
        ));
        match call {
            AvailCall::Delete => store.delete_avail(&job, ExpiresAt::Never).await?,
            AvailCall::DeleteByJob => store.delete_avail_by_job(&job).await?,
            AvailCall::Redate => assert_eq!(
                store
                    .redate_avail(&job, ExpiresAt::Never, ExpiresAt::At(at))
                    .await?,
                RedateResult::Gone
            ),
        }
        assert!(store.get_avail(&job, ExpiresAt::Never).await?.is_none());
        assert!(store.get_avail_by_job(&job).await?.is_none());
        assert!(store.avail_key_for_job(&job).await?.is_none());
        assert!(
            store
                .get_leased(&job, ExpiresAt::Never, &client, at)
                .await?
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn stale_keys_do_not_change_the_live_job() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("stale-key-job")?;
        let client = ClientId::try_new("stale_client")?;
        let at = now()?;
        let exp = ExpiresAt::At(at);
        enqueue(&store, &job).await?;
        assert!(store.get_avail(&job, exp).await?.is_none());
        store.delete_avail(&job, exp).await?;
        assert!(matches!(
            store.claim_job(&job, exp, &client, at).await?,
            ClaimResult::Gone
        ));
        assert!(matches!(
            store.claim_job(&job, ExpiresAt::Never, &client, at).await?,
            ClaimResult::Claimed(_)
        ));
        assert!(matches!(
            store.claim_job(&job, ExpiresAt::Never, &client, at).await?,
            ClaimResult::Claimed(_)
        ));
        let other = ClientId::try_new("stale_other")?;
        assert!(matches!(
            store.claim_job(&job, ExpiresAt::Never, &other, at).await?,
            ClaimResult::Gone
        ));
        let renewed = at + chrono::TimeDelta::seconds(60);
        assert_eq!(
            store.renew_lease(&job, &client, renewed).await?,
            RenewLeaseResult::Renewed(ExpiresAt::Never)
        );
        assert!(
            store
                .get_leased(&job, ExpiresAt::Never, &client, at)
                .await?
                .is_none()
        );
        store
            .delete_lease(&job, ExpiresAt::Never, &client, at)
            .await?;
        assert_eq!(
            store
                .recycle_lease(&job, ExpiresAt::Never, &client, at)
                .await?,
            RecycleResult::Gone
        );
        assert_eq!(
            store
                .recycle_lease(&job, ExpiresAt::Never, &client, renewed)
                .await?,
            RecycleResult::Recycled
        );
        assert_eq!(
            store.redate_avail(&job, ExpiresAt::Never, exp).await?,
            RedateResult::Redated
        );
        assert_eq!(
            store.avail_key_for_job(&job).await?,
            Some(avail_filename(&job, exp))
        );
        assert!(store.get_avail(&job, exp).await?.is_some());
        assert_eq!(
            store
                .recycle_lease(&job, ExpiresAt::Never, &client, renewed)
                .await?,
            RecycleResult::Gone
        );
        Ok(())
    }

    #[tokio::test]
    async fn promote_retries_and_conflicts_keep_the_queued_body() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("promote-job")?;
        enqueue(&store, &job).await?;
        let body = store.get_avail_by_job(&job).await?;
        store.promote_avail(&job, ExpiresAt::Never).await?;
        assert!(
            store
                .promote_avail(&job, ExpiresAt::At(now()?))
                .await
                .is_err()
        );
        store
            .write_tmp(&job, &serde_json::json!({"replacement": true}))
            .await?;
        store.promote_avail(&job, ExpiresAt::At(now()?)).await?;
        assert_eq!(store.get_avail_by_job(&job).await?, body);
        assert_eq!(
            store.avail_key_for_job(&job).await?,
            Some(avail_filename(&job, ExpiresAt::Never))
        );
        assert!(store.list_stale_tmp(Duration::ZERO).await?.is_empty());
        Ok(())
    }

    #[derive(Debug)]
    enum Step {
        Claim,
        Renew,
        Recycle,
        Redate,
    }

    #[tokio::test]
    async fn every_write_bumps_the_revision() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        let store = SqlTodoStore::new(db.clone());
        let job = JobId::try_new("revision-job")?;
        let client = ClientId::try_new("revision_client")?;
        let at = now()?;
        enqueue(&store, &job).await?;
        let revision = || {
            sqlx::query_scalar::<_, i64>("SELECT revision FROM queue_jobs WHERE job_id = $1")
                .bind(job.as_str())
                .fetch_one(db.pool())
        };
        let mut previous = revision().await?;
        for step in [Step::Claim, Step::Renew, Step::Recycle, Step::Redate] {
            match step {
                Step::Claim => {
                    assert!(matches!(
                        store.claim_job(&job, ExpiresAt::Never, &client, at).await?,
                        ClaimResult::Claimed(_)
                    ));
                }
                Step::Renew => assert_eq!(
                    store
                        .renew_lease(&job, &client, at + chrono::TimeDelta::seconds(60))
                        .await?,
                    RenewLeaseResult::Renewed(ExpiresAt::Never)
                ),
                Step::Recycle => assert_eq!(
                    store
                        .recycle_lease(
                            &job,
                            ExpiresAt::Never,
                            &client,
                            at + chrono::TimeDelta::seconds(60)
                        )
                        .await?,
                    RecycleResult::Recycled
                ),
                Step::Redate => assert_eq!(
                    store
                        .redate_avail(&job, ExpiresAt::Never, ExpiresAt::At(at))
                        .await?,
                    RedateResult::Redated
                ),
            }
            let current = revision().await?;
            assert_eq!(current, previous + 1, "{step:?}");
            previous = current;
        }
        Ok(())
    }
}
