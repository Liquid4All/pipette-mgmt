//! PostgreSQL storage for queue jobs, leases and markers.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};

use pipette_core::denial::DenialRecord;
use pipette_core::todo_filename::Lease;
use pipette_plan_types::{
    CachedArtifacts, CampaignId, ClientId, ExpiresAt, FailureCode, JobId, PlanStatus,
};
use pipette_store::{
    AvailEntry, CampaignBacklog, ClaimAffinity, ClaimOutcome, ClaimResult, ReclaimOutcome,
    RecycleResult, RedateResult, ReindexBatch, ReindexJob, ReindexRequest, RenewLeaseResult,
    TodoStore,
};

use crate::retry::retry;
use crate::{SqlDatabase, plans::first_revision};

/// Queue storage on PostgreSQL.
#[derive(Clone, Debug)]
pub struct SqlTodoStore {
    db: SqlDatabase,
    body_reads: Arc<AtomicU64>,
}

impl SqlTodoStore {
    pub fn new(db: SqlDatabase) -> Self {
        Self {
            db,
            body_reads: Arc::new(AtomicU64::new(0)),
        }
    }

    fn record_body_reads(&self, count: usize) {
        self.body_reads
            .fetch_add(u64::try_from(count).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

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

/// Serializes one client's claims until the transaction ends, so the client cannot
/// acquire two leases at once. Two clients whose IDs hash alike only
/// wait for each other. Lock the parent before settlement touches grant rows,
/// in the same order as cascading client deletion.
async fn lock_client(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    client: &str,
) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(client)
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT client_id FROM clients WHERE client_id = $1 FOR KEY SHARE")
        .bind(client)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Recycle lapsed leases under the client lock. Terminal records and pending
/// repairs need object-store work; keep those jobs leased until it completes.
/// Records a lost lease. The client may not hold the job's artifacts after a
/// lost lease, so the statement also forgets the client's last grant of the job.
const RECORD_LOST_LEASE: &str = "WITH forgot AS (DELETE FROM queue_client_last_grant WHERE \
     client_id = $2 AND job_id = $1) INSERT INTO queue_lost_leases (job_id, client_id, \
     lease_expiry) SELECT job_id, $2, $3 FROM queue_jobs WHERE job_id = $1 \
     ON CONFLICT DO NOTHING";

/// Makes the job the client's last grant, with the job's artifact keys. A claim
/// grant and a reclaim both run it, because after either one the client runs
/// the job. Lock the client row until commit; a deleted client stores no grant.
const RECORD_LAST_GRANT: &str = "INSERT INTO queue_client_last_grant (client_id, job_id, \
     model_key, runtime_key, granted_at) SELECT c.client_id, j.job_id, j.model_key, \
     j.runtime_key, $3 FROM queue_jobs j JOIN clients c ON c.client_id = $2 \
     WHERE j.job_id = $1 FOR KEY SHARE OF c ON CONFLICT (client_id) DO UPDATE SET job_id = \
     EXCLUDED.job_id, model_key = EXCLUDED.model_key, runtime_key = EXCLUDED.runtime_key, \
     granted_at = EXCLUDED.granted_at";

/// The model and runtime keys that the client holds. `queue_json_key` gives
/// both an advertised descriptor's key and the `queue_jobs` key columns, so
/// they match exactly. A descriptor that jsonb cannot represent has no key
/// and matches no job. Without an advertised set, the keys of the client's
/// last grant.
async fn cached_keys(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    client: &str,
    cached: Option<&CachedArtifacts>,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let keys = match cached {
        Some(cached) => {
            sqlx::query_as(
                "SELECT array_remove(ARRAY(SELECT queue_json_key(d::text, '{}') FROM \
                 json_array_elements($1::json) d), NULL), array_remove(ARRAY(SELECT \
                 queue_json_key(d::text, '{}') FROM json_array_elements($2::json) d), NULL)",
            )
            .bind(serde_json::to_string(&cached.models)?)
            .bind(serde_json::to_string(&cached.runtimes)?)
            .fetch_one(&mut **tx)
            .await?
        }
        None => {
            let last: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT model_key, runtime_key FROM queue_client_last_grant WHERE client_id = $1",
            )
            .bind(client)
            .fetch_optional(&mut **tx)
            .await?;
            let (model, runtime) = last.unwrap_or_default();
            (Vec::from_iter(model), Vec::from_iter(runtime))
        }
    };
    Ok(keys)
}

async fn settle_lapsed_in_transaction(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    client: &str,
    except: Option<&str>,
    lost_lease_limit: usize,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let mut lapsed: Vec<(String, DateTime<Utc>, bool, bool)> = sqlx::query_as(
        "SELECT j.job_id, j.lease_expiry, \
         EXISTS (SELECT 1 FROM submission_outcomes s WHERE s.job_id = j.job_id), \
         EXISTS (SELECT 1 FROM submission_outcome_repairs r \
                 WHERE r.job_id = j.job_id AND r.pending) \
         FROM queue_jobs j WHERE j.leased_by = $1 AND j.lease_expiry <= $2 \
         AND ($3::text IS NULL OR j.job_id <> $3) ORDER BY j.job_id FOR UPDATE OF j",
    )
    .bind(client)
    .bind(now)
    .bind(except)
    .fetch_all(&mut **tx)
    .await?;
    let mut needs_settlement = false;
    while let Some((job, lease_expiry, recorded, pending)) = lapsed.pop() {
        if pending {
            needs_settlement = true;
            continue;
        }
        if recorded {
            sqlx::query("DELETE FROM queue_jobs WHERE job_id = $1")
                .bind(&job)
                .execute(&mut **tx)
                .await?;
            continue;
        }
        sqlx::query(RECORD_LOST_LEASE)
            .bind(&job)
            .bind(client)
            .bind(lease_expiry)
            .execute(&mut **tx)
            .await?;
        if lost_lease_limit > 0 {
            let (losses,): (i64,) =
                sqlx::query_as("SELECT count(*) FROM queue_lost_leases WHERE job_id = $1")
                    .bind(&job)
                    .fetch_one(&mut **tx)
                    .await?;
            if usize::try_from(losses).unwrap_or(usize::MAX) >= lost_lease_limit {
                needs_settlement = true;
                continue;
            }
        }
        sqlx::query(
            "UPDATE queue_jobs SET leased_by = NULL, lease_expiry = NULL, \
             needs_reindex = true, revision = revision + 1 WHERE job_id = $1",
        )
        .bind(&job)
        .execute(&mut **tx)
        .await?;
    }
    Ok(needs_settlement)
}

fn valid_campaign(raw: &str) -> Option<CampaignId> {
    CampaignId::try_new(raw)
        .inspect_err(|error| {
            tracing::warn!(campaign_id = raw, %error, "skipping a backlog campaign with an invalid id");
        })
        .ok()
}

/// A SQL `count(*)`, which is never negative.
fn count(rows: i64) -> u64 {
    u64::try_from(rows).unwrap_or_default()
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
    fn lease(self) -> Option<Lease> {
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
        Some(Lease {
            job_id: job,
            expires_at: deadline(self.expires_at),
            client_id: client,
            lease_expiry: self.lease_expiry,
        })
    }
}

#[async_trait]
impl TodoStore for SqlTodoStore {
    fn queue_body_reads(&self) -> u64 {
        self.body_reads.load(Ordering::Relaxed)
    }

    async fn avail_for_jobs(&self, jobs: &[JobId]) -> anyhow::Result<Vec<AvailEntry>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<_> = jobs.iter().map(JobId::as_str).collect();
        let rows: Vec<(String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("avail_for_jobs", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at FROM queue_jobs \
                     WHERE job_id = ANY($1) AND leased_by IS NULL",
                )
                .bind(&ids)
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(job, exp)| {
                Some(AvailEntry {
                    job_id: valid_job(&job)?,
                    expires_at: deadline(exp),
                })
            })
            .collect())
    }

    async fn leases_for_jobs(&self, jobs: &[JobId]) -> anyhow::Result<Vec<Lease>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<_> = jobs.iter().map(JobId::as_str).collect();
        let rows: Vec<LeaseRow> = self
            .db
            .fetch_all("leases_for_jobs", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at, leased_by, lease_expiry FROM queue_jobs \
                     WHERE job_id = ANY($1) AND leased_by IS NOT NULL",
                )
                .bind(&ids)
            })
            .await?;
        Ok(rows.into_iter().filter_map(LeaseRow::lease).collect())
    }

    async fn denials_for_jobs(&self, jobs: &[JobId]) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<_> = jobs.iter().map(JobId::as_str).collect();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("denials_for_jobs", || {
                sqlx::query_as("SELECT job_id, client_id FROM queue_denials WHERE job_id = ANY($1)")
                    .bind(&ids)
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(job, client)| Some((valid_job(&job)?, valid_client(&client)?)))
            .collect())
    }

    async fn leases_for_client(&self, client: &ClientId) -> anyhow::Result<Vec<Lease>> {
        let rows: Vec<LeaseRow> = self
            .db
            .fetch_all("leases_for_client", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at, leased_by, lease_expiry FROM queue_jobs \
                     WHERE leased_by = $1",
                )
                .bind(client.as_str())
            })
            .await?;
        Ok(rows.into_iter().filter_map(LeaseRow::lease).collect())
    }

    async fn list_avail_entries(&self) -> anyhow::Result<Vec<AvailEntry>> {
        let rows: Vec<(String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("list_avail_entries", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at FROM queue_jobs WHERE leased_by IS NULL \
                     ORDER BY job_id COLLATE \"C\"",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(job, exp)| {
                Some(AvailEntry {
                    job_id: valid_job(&job)?,
                    expires_at: deadline(exp),
                })
            })
            .collect())
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
        self.record_body_reads(usize::from(row.is_some()));
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
        self.record_body_reads(usize::from(row.is_some()));
        row.map(|(body,)| parse_body(job_id, &body)).transpose()
    }

    async fn list_leases(&self) -> anyhow::Result<Vec<Lease>> {
        let rows: Vec<LeaseRow> = self
            .db
            .fetch_all("list_leases", || {
                sqlx::query_as(
                    "SELECT job_id, expires_at, leased_by, lease_expiry FROM \
                     queue_jobs WHERE leased_by IS NOT NULL",
                )
            })
            .await?;
        Ok(rows.into_iter().filter_map(LeaseRow::lease).collect())
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
        self.record_body_reads(usize::from(row.is_some()));
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
        self.record_body_reads(usize::from(row.is_some()));
        match row {
            Some((body,)) => Ok(ClaimResult::Claimed(parse_body(job_id, &body)?)),
            None => Ok(ClaimResult::Gone),
        }
    }

    /// One `READ COMMITTED` transaction, without a retry. `SKIP LOCKED` skips jobs that
    /// concurrent claims lock, so the claim cannot fail with `40001`.
    async fn claim_for_client(
        &self,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
        lost_lease_limit: usize,
        passed_over: &[JobId],
        checked_lease: Option<&Lease>,
        cached: Option<&CachedArtifacts>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ClaimOutcome> {
        let client = client_id.as_str();
        let passed_over: Vec<&str> = passed_over.iter().map(JobId::as_str).collect();
        let lease_expiry = to_second(lease_expiry);
        let mut tx = self.db.begin_read_committed().await?;
        lock_client(&mut tx, client).await?;

        if settle_lapsed_in_transaction(&mut tx, client, None, lost_lease_limit, now).await? {
            tx.commit().await?;
            return Ok(ClaimOutcome::SettlementRequired);
        }

        let held: Option<(String, String, Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT job_id, body, expires_at, lease_expiry FROM queue_jobs WHERE leased_by \
                 = $1 AND lease_expiry > $2 ORDER BY lease_expiry DESC LIMIT 1",
        )
        .bind(client)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        self.record_body_reads(usize::from(held.is_some()));
        let outcome = if let Some((job, body, expires_at, held_until)) = held {
            let job_id = valid_job(&job).context("the held queue row has an invalid job id")?;
            let body = parse_body(&job_id, &body)?;
            let lease = Lease {
                job_id,
                expires_at: deadline(expires_at),
                client_id: client_id.clone(),
                lease_expiry: held_until,
            };
            if checked_lease != Some(&lease) {
                tx.commit().await?;
                return Ok(ClaimOutcome::CheckHeldLease { lease, body });
            }
            let withheld = if lost_lease_limit > 0 {
                sqlx::query(RECORD_LOST_LEASE)
                    .bind(&job)
                    .bind(client)
                    .bind(held_until)
                    .execute(&mut *tx)
                    .await?;
                let (losses,): (i64,) =
                    sqlx::query_as("SELECT count(*) FROM queue_lost_leases WHERE job_id = $1")
                        .bind(&job)
                        .fetch_one(&mut *tx)
                        .await?;
                usize::try_from(losses).unwrap_or(usize::MAX) >= lost_lease_limit
            } else {
                false
            };
            if withheld {
                ClaimOutcome::Withheld(lease)
            } else {
                ClaimOutcome::HandedBack { lease, body }
            }
        } else {
            let (models, runtimes) = cached_keys(&mut tx, client, cached).await?;
            // `is_expired` keeps a job whose deadline is exactly `now`. Within one
            // deadline: the fewer smaller runs of its family the queue holds, leased
            // or not, the sooner a job goes, so a device takes a larger run only when
            // it has nothing ranked before it. Then the model the fewest other clients
            // lease, so devices spread across the models. Then a job whose model and
            // runtime the client holds, then one whose model it holds, then one whose
            // runtime it holds, then any job.
            let pick: Option<(String, String, Option<DateTime<Utc>>, bool, bool)> = sqlx::query_as(
                "WITH busy AS (SELECT l.model_key, count(*) AS leases FROM queue_jobs l WHERE \
                     l.leased_by IS NOT NULL AND l.leased_by <> $1 AND l.lease_expiry > $2 AND \
                     EXISTS (SELECT 1 FROM queue_eligibility e WHERE e.client_id = $1 AND \
                     e.job_id = l.job_id) GROUP BY l.model_key) SELECT j.job_id, j.body, \
                     j.expires_at, (j.model_key = ANY($4)) IS TRUE AS model_held, \
                     (j.runtime_key = ANY($5)) IS TRUE AS runtime_held FROM queue_eligibility e \
                     JOIN queue_jobs j ON j.job_id = e.job_id LEFT JOIN busy b \
                     ON b.model_key = j.model_key WHERE \
                     e.client_id = $1 AND j.leased_by IS NULL AND (j.expires_at IS NULL OR \
                     j.expires_at >= $2) AND NOT EXISTS (SELECT 1 FROM queue_denials d WHERE \
                     d.job_id = j.job_id AND d.client_id = $1) AND NOT (j.job_id = ANY($3)) \
                     ORDER BY j.expires_at ASC NULLS LAST, (SELECT count(*) FROM queue_jobs s \
                     WHERE s.family_key = j.family_key AND queue_sizes_below(s.sizes, j.sizes)) \
                     ASC, COALESCE(b.leases, 0) ASC, model_held DESC, runtime_held DESC, \
                     random() LIMIT 1 FOR UPDATE OF j SKIP LOCKED",
            )
            .bind(client)
            .bind(now)
            .bind(&passed_over)
            .bind(&models)
            .bind(&runtimes)
            .fetch_optional(&mut *tx)
            .await?;
            self.record_body_reads(usize::from(pick.is_some()));
            match pick {
                Some((job, body, expires_at, model_held, runtime_held)) => {
                    sqlx::query(
                        "UPDATE queue_jobs SET leased_by = $2, lease_expiry = $3, revision = \
                         revision + 1 WHERE job_id = $1",
                    )
                    .bind(&job)
                    .bind(client)
                    .bind(lease_expiry)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(RECORD_LAST_GRANT)
                        .bind(&job)
                        .bind(client)
                        .bind(now)
                        .execute(&mut *tx)
                        .await?;
                    let job_id =
                        valid_job(&job).context("the picked queue row has an invalid job id")?;
                    let body = parse_body(&job_id, &body)?;
                    let lease = Lease {
                        job_id,
                        expires_at: deadline(expires_at),
                        client_id: client_id.clone(),
                        lease_expiry,
                    };
                    ClaimOutcome::Granted {
                        lease,
                        body,
                        affinity: ClaimAffinity::new(model_held, runtime_held),
                    }
                }
                None => ClaimOutcome::Empty,
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// One `READ COMMITTED` transaction, as [`Self::claim_for_client`]'s.
    async fn reclaim_for_client(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
        lost_lease_limit: usize,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ReclaimOutcome> {
        let job = job_id.as_str();
        let client = client_id.as_str();
        let lease_expiry = to_second(lease_expiry);
        let mut tx = self.db.begin_read_committed().await?;
        lock_client(&mut tx, client).await?;

        if settle_lapsed_in_transaction(&mut tx, client, Some(job), lost_lease_limit, now).await? {
            tx.commit().await?;
            return Ok(ReclaimOutcome::SettlementRequired);
        }

        let renewed = sqlx::query(
            "UPDATE queue_jobs SET lease_expiry = $3, revision = revision + 1 WHERE job_id = $1 \
             AND leased_by = $2",
        )
        .bind(job)
        .bind(client)
        .bind(lease_expiry)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        let outcome = if renewed {
            ReclaimOutcome::Renewed
        } else {
            let holder: Option<Option<String>> =
                sqlx::query_scalar("SELECT leased_by FROM queue_jobs WHERE job_id = $1")
                    .bind(job)
                    .fetch_optional(&mut *tx)
                    .await?;
            if holder.flatten().is_some() {
                ReclaimOutcome::LeasedElsewhere
            } else {
                let granted: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
                    "UPDATE queue_jobs j SET leased_by = $2, lease_expiry = $3, revision = \
                     revision + 1 WHERE j.job_id = $1 AND j.leased_by IS NULL AND (j.expires_at \
                     IS NULL OR j.expires_at >= $4) AND EXISTS (SELECT 1 FROM queue_eligibility \
                     e WHERE e.client_id = $2 AND e.job_id = $1) AND NOT EXISTS (SELECT 1 FROM \
                     queue_denials d WHERE d.job_id = $1 AND d.client_id = $2) AND NOT EXISTS \
                     (SELECT 1 FROM queue_jobs o WHERE o.leased_by = $2 AND o.lease_expiry > $4) \
                     RETURNING j.body, j.expires_at",
                )
                .bind(job)
                .bind(client)
                .bind(lease_expiry)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await?;
                self.record_body_reads(usize::from(granted.is_some()));
                match granted {
                    Some((body, expires_at)) => ReclaimOutcome::Granted {
                        lease: Lease {
                            job_id: job_id.clone(),
                            expires_at: deadline(expires_at),
                            client_id: client_id.clone(),
                            lease_expiry,
                        },
                        body: parse_body(job_id, &body)?,
                    },
                    None => ReclaimOutcome::Refused,
                }
            }
        };
        if matches!(
            outcome,
            ReclaimOutcome::Renewed | ReclaimOutcome::Granted { .. }
        ) {
            sqlx::query(RECORD_LAST_GRANT)
                .bind(job)
                .bind(client)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(outcome)
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
                    "UPDATE queue_jobs SET expires_at = $3, needs_reindex = true, revision = \
                     revision + 1 WHERE job_id = $1 AND leased_by IS NULL AND expires_at IS \
                     NOT DISTINCT FROM $2",
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

    async fn restamp_avail_deadlines(&self, now: DateTime<Utc>) -> anyhow::Result<u64> {
        // `infinity` stands for `never`, so it ranks after every date. Unlike a
        // re-date, no reindex flag: an eligibility row holds no deadline. Only
        // rows `marker_of` parses count, so a bad row cannot fail the cast. The
        // markers are read through the waiting jobs, as `job_plans` keeps every
        // plan that ever held a job.
        self.db
            .execute("restamp_avail_deadlines", || {
                sqlx::query(
                    "WITH member AS (SELECT p.job_id, max(CASE p.expires_at WHEN 'never' THEN \
                     'infinity' ELSE p.expires_at END::timestamptz) AS horizon FROM job_plans p \
                     JOIN queue_jobs q ON q.job_id = p.job_id AND q.leased_by IS NULL \
                     WHERE p.plan_id ~ '^[A-Za-z0-9-]+$' AND (p.expires_at = 'never' OR \
                     (p.expires_at ~ '^[0-9]{8}T[0-9]{6}Z$' AND \
                     pg_input_is_valid(p.expires_at, 'timestamptz'))) \
                     GROUP BY p.job_id, p.plan_id), earliest AS (SELECT job_id, NULLIF(min(horizon), \
                     'infinity') AS deadline FROM member WHERE horizon >= $1 GROUP BY job_id) \
                     UPDATE queue_jobs j SET expires_at = e.deadline, revision = j.revision + 1 \
                     FROM earliest e WHERE j.job_id = e.job_id AND j.leased_by IS NULL AND \
                     j.expires_at IS DISTINCT FROM e.deadline",
                )
                .bind(now)
            })
            .await
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
                     lease_expiry = NULL, needs_reindex = true, revision = revision + 1 \
                     WHERE job_id = $1 AND expires_at IS NOT DISTINCT FROM $2 AND leased_by = $3 AND \
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

    async fn forget_last_grant(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("forget_last_grant", || {
                sqlx::query(
                    "DELETE FROM queue_client_last_grant WHERE client_id = $1 AND job_id = $2",
                )
                .bind(client_id.as_str())
                .bind(job_id.as_str())
            })
            .await?;
        Ok(())
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
                    "WITH flagged AS (UPDATE queue_jobs SET needs_reindex = true, \
                     revision = revision + 1 WHERE job_id = $1) \
                     INSERT INTO queue_denials (job_id, client_id, denied_at, failure_reason, \
                     failure_code, client_version, runtime_version) SELECT job_id, $2, $3, $4, \
                     $5, $6, $7 FROM queue_jobs WHERE job_id = $1 ON CONFLICT (job_id, \
                     client_id) DO UPDATE SET denied_at = \
                     EXCLUDED.denied_at, failure_reason = EXCLUDED.failure_reason, failure_code = \
                     EXCLUDED.failure_code, client_version = EXCLUDED.client_version, \
                     runtime_version = EXCLUDED.runtime_version",
                )
                .bind(job_id.as_str())
                .bind(client_id.as_str())
                .bind(record.denied_at)
                .bind(record.failure_reason.as_deref())
                .bind(record.failure_code.as_ref())
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
            Option<String>,
        )> = self
            .db
            .fetch_optional("get_denied", || {
                sqlx::query_as(
                    "SELECT denied_at, failure_reason, failure_code, client_version, \
                     runtime_version FROM queue_denials WHERE job_id = $1 AND client_id = $2",
                )
                .bind(job_id.as_str())
                .bind(client_id.as_str())
            })
            .await?;
        row.map(
            |(denied_at, failure_reason, failure_code, client_version, runtime_version)| {
                Ok(DenialRecord {
                    denied_at,
                    failure_reason,
                    // A row written before the column holds NULL.
                    failure_code: failure_code
                        .map(|code| {
                            code.parse::<FailureCode>()
                                .with_context(|| format!("queue_denials.failure_code {code:?}"))
                        })
                        .transpose()?
                        .unwrap_or_default(),
                    client_version,
                    runtime_version,
                })
            },
        )
        .transpose()
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
                    "SELECT e.job_id, j.expires_at FROM queue_eligibility e LEFT JOIN \
                     queue_jobs j USING (job_id) WHERE e.client_id = $1 ORDER BY e.job_id",
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
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.db
            .execute("write_eligible", || {
                sqlx::query(
                    "INSERT INTO queue_eligibility (client_id, job_id) SELECT $1, job_id FROM \
                     queue_jobs WHERE job_id = $2 ON CONFLICT DO NOTHING",
                )
                .bind(client_id.as_str())
                .bind(job_id.as_str())
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
        _expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.db
            .execute("delete_eligible", || {
                sqlx::query("DELETE FROM queue_eligibility WHERE client_id = $1 AND job_id = $2")
                    .bind(client_id.as_str())
                    .bind(job_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        let rows: Vec<(String, String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("list_all_eligible", || {
                sqlx::query_as(
                    "SELECT e.client_id, e.job_id, j.expires_at FROM queue_eligibility e LEFT \
                     JOIN queue_jobs j USING (job_id) ORDER BY e.client_id, e.job_id",
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

    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<ReindexRequest>> {
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
            .filter_map(|(c, request_id)| {
                Some(ReindexRequest {
                    client_id: valid_client(&c)?,
                    request_id,
                })
            })
            .collect())
    }

    async fn delete_pending_reindex(&self, request: &ReindexRequest) -> anyhow::Result<()> {
        self.db
            .execute("delete_pending_reindex", || {
                sqlx::query("DELETE FROM queue_reindex_requests WHERE request_id = $1")
                    .bind(request.request_id.as_str())
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

    async fn write_lost_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.db
            .execute("write_lost_lease", || {
                sqlx::query(RECORD_LOST_LEASE)
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

    async fn promote_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        family: Option<(&str, &[i64])>,
    ) -> anyhow::Result<()> {
        let revision = first_revision();
        let (moved, inserted, present): (bool, bool, bool) = self
            .db
            .fetch_one("promote queue job", || {
                sqlx::query_as(
                    "WITH moved AS (DELETE FROM queue_tmp WHERE job_id = $1 RETURNING \
                     body), ins AS (INSERT INTO queue_jobs (job_id, body, expires_at, \
                     revision, family_key, sizes) SELECT $1, body, $2, $3, $4, $5 \
                     FROM moved ON CONFLICT DO NOTHING RETURNING 1) SELECT EXISTS (SELECT 1 \
                     FROM moved), EXISTS (SELECT 1 FROM ins), EXISTS (SELECT 1 FROM \
                     queue_jobs WHERE job_id = $1 AND expires_at IS NOT DISTINCT FROM $2)",
                )
                .bind(job_id.as_str())
                .bind(expiry(expires_at))
                .bind(revision)
                .bind(family.map(|(key, _)| key))
                .bind(family.map(|(_, sizes)| sizes))
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

    async fn queued_larger_runs(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(AvailEntry, serde_json::Value)>> {
        let rows: Vec<(String, String, Option<DateTime<Utc>>)> = self
            .db
            .fetch_all("queued_larger_runs", || {
                sqlx::query_as(
                    "SELECT s.job_id, s.body, s.expires_at FROM queue_jobs s JOIN queue_jobs f \
                     ON f.job_id = $1 WHERE s.family_key = f.family_key AND \
                     queue_sizes_below(f.sizes, s.sizes) AND s.leased_by IS NULL",
                )
                .bind(job_id.as_str())
            })
            .await?;
        self.record_body_reads(rows.len());
        rows.into_iter()
            .filter_map(|(job, body, exp)| Some((valid_job(&job)?, body, exp)))
            .map(|(job_id, body, exp)| {
                let body = parse_body(&job_id, &body)?;
                Ok((
                    AvailEntry {
                        job_id,
                        expires_at: deadline(exp),
                    },
                    body,
                ))
            })
            .collect()
    }

    async fn delete_stale_tmp(&self, age: Duration) -> anyhow::Result<u64> {
        self.db
            .execute("delete_stale_tmp", || {
                sqlx::query(
                    "DELETE FROM queue_tmp WHERE written_at <= now() - $1 * interval '1 second'",
                )
                .bind(age.as_secs_f64())
            })
            .await
    }

    async fn backlog(&self) -> anyhow::Result<BTreeMap<ClientId, u64>> {
        // The conditions are the claim's: `>=` keeps a job whose deadline is
        // exactly now.
        let rows: Vec<(String, i64)> = self
            .db
            .fetch_all("backlog", || {
                sqlx::query_as(
                    "SELECT e.client_id, count(*) FROM queue_eligibility e JOIN queue_jobs j ON \
                     j.job_id = e.job_id WHERE j.leased_by IS NULL AND (j.expires_at IS NULL OR \
                     j.expires_at >= now()) AND NOT EXISTS (SELECT 1 FROM queue_denials d WHERE \
                     d.job_id = e.job_id AND d.client_id = e.client_id) GROUP BY e.client_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(client, jobs)| Some((valid_client(&client)?, count(jobs))))
            .collect())
    }

    async fn campaign_backlog(
        &self,
        client_id: &ClientId,
        creating_before: DateTime<Utc>,
    ) -> anyhow::Result<CampaignBacklog> {
        // A membership's deadline is stored as text, accurate to the second.
        let now = ExpiresAt::At(Utc::now()).to_string();
        let ended = [
            PlanStatus::Cancelling.label(),
            PlanStatus::Cancelled.label(),
        ];
        // One row for each campaign, and one row with no campaign for the jobs
        // that no campaign holds.
        let rows: Vec<(Option<String>, i64)> = self
            .db
            .fetch_all("campaign_backlog", || {
                sqlx::query_as(
                    "WITH claimable AS (SELECT e.job_id FROM queue_eligibility e JOIN queue_jobs \
                     j ON j.job_id = e.job_id WHERE e.client_id = $1 AND j.leased_by IS NULL AND \
                     (j.expires_at IS NULL OR j.expires_at >= now()) AND NOT EXISTS (SELECT 1 \
                     FROM queue_denials d WHERE d.job_id = e.job_id AND d.client_id = $1)), \
                     held AS (SELECT DISTINCT c.job_id, p.campaign_id FROM claimable c JOIN \
                     job_plans m ON m.job_id = c.job_id JOIN plans p ON p.plan_id = m.plan_id \
                     WHERE p.campaign_id IS NOT NULL AND NOT (p.status = ANY($2)) AND NOT EXISTS \
                     (SELECT 1 FROM cancelled_plans x WHERE x.plan_id = m.plan_id) AND \
                     (m.expires_at = 'never' OR m.expires_at >= $3) AND NOT (p.status = $4 AND \
                     CASE WHEN p.body IS JSON OBJECT THEN \
                       CASE WHEN pg_input_is_valid(p.body::json ->> 'created_at', 'timestamptz') \
                         THEN (p.body::json ->> 'created_at')::timestamptz <= $5 \
                         ELSE false END \
                       ELSE false END)) \
                     SELECT campaign_id, count(*) FROM held GROUP BY campaign_id UNION ALL \
                     SELECT NULL, count(*) FROM claimable c WHERE NOT EXISTS (SELECT 1 FROM held \
                     h WHERE h.job_id = c.job_id)",
                )
                .bind(client_id.as_str())
                .bind(ended)
                .bind(now.as_str())
                .bind(PlanStatus::Creating.label())
                .bind(creating_before)
            })
            .await?;
        Ok(rows.into_iter().fold(
            CampaignBacklog::default(),
            |mut backlog, (campaign, jobs)| {
                match campaign {
                    None => backlog.outside_campaigns = count(jobs),
                    Some(raw) => {
                        if let Some(campaign_id) = valid_campaign(&raw) {
                            backlog.by_campaign.insert(campaign_id, count(jobs));
                        }
                    }
                }
                backlog
            },
        ))
    }

    async fn jobs_needing_reindex(&self, jobs: &[JobId]) -> anyhow::Result<ReindexBatch> {
        if jobs.is_empty() {
            return Ok(ReindexBatch::default());
        }
        let jobs: Vec<&str> = jobs.iter().map(JobId::as_str).collect();
        let rows: Vec<(String, String, Option<DateTime<Utc>>, i64)> = self
            .db
            .fetch_all("jobs_needing_reindex", || {
                sqlx::query_as(
                    "SELECT job_id, body, expires_at, revision FROM queue_jobs WHERE \
                     job_id = ANY($1) AND needs_reindex AND leased_by IS NULL ORDER BY job_id",
                )
                .bind(jobs.clone())
            })
            .await?;
        self.record_body_reads(rows.len());
        Ok(rows
            .into_iter()
            .filter_map(|(j, body, exp, revision)| Some((valid_job(&j)?, body, exp, revision)))
            .fold(
                ReindexBatch::default(),
                |mut batch, (job_id, body, exp, revision)| {
                    match parse_body(&job_id, &body) {
                        Ok(body) => batch.jobs.push(ReindexJob {
                            job_id,
                            expires_at: deadline(exp),
                            body,
                            revision,
                        }),
                        Err(error) => {
                            tracing::warn!(
                                job_id = %job_id,
                                error = %error,
                                "queue: flagged job body does not parse; the job keeps its flag"
                            );
                            batch.unreadable.push(job_id);
                        }
                    }
                    batch
                },
            ))
    }

    async fn avail_clause_groups(
        &self,
        jobs: &[JobId],
    ) -> anyhow::Result<Vec<(serde_json::Value, Vec<JobId>)>> {
        let jobs: Vec<&str> = jobs.iter().map(JobId::as_str).collect();
        let rows: Vec<(serde_json::Value, Vec<String>)> = self
            .db
            .fetch_all("avail_clause_groups", || {
                sqlx::query_as(
                    "SELECT match_clauses, array_agg(job_id ORDER BY job_id) FROM queue_jobs \
                     WHERE job_id = ANY($1) AND leased_by IS NULL AND match_clauses IS NOT NULL \
                     GROUP BY match_clauses",
                )
                .bind(jobs.clone())
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(clauses, jobs)| {
                (
                    clauses,
                    jobs.iter().filter_map(|job| valid_job(job)).collect(),
                )
            })
            .collect())
    }

    async fn write_eligible_rows(&self, client: &ClientId, jobs: &[JobId]) -> anyhow::Result<()> {
        let jobs: Vec<&str> = jobs.iter().map(JobId::as_str).collect();
        self.db
            .execute("write_eligible_rows", || {
                sqlx::query(
                    "INSERT INTO queue_eligibility (client_id, job_id) SELECT $1, job_id FROM \
                     queue_jobs WHERE job_id = ANY($2) ON CONFLICT DO NOTHING",
                )
                .bind(client.as_str())
                .bind(jobs.clone())
            })
            .await?;
        Ok(())
    }

    async fn replace_eligibility(
        &self,
        job_id: &JobId,
        revision: i64,
        clients: &[ClientId],
    ) -> anyhow::Result<bool> {
        let clients: Vec<&str> = clients.iter().map(ClientId::as_str).collect();
        Ok(retry("replace_eligibility", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let current: Option<(i64,)> = sqlx::query_as(
                    "SELECT revision FROM queue_jobs WHERE job_id = $1 AND revision = $2 FOR \
                     UPDATE",
                )
                .bind(job_id.as_str())
                .bind(revision)
                .fetch_optional(&mut *tx)
                .await?;
                if current.is_none() {
                    return Ok(false);
                }
                sqlx::query("DELETE FROM queue_eligibility WHERE job_id = $1")
                    .bind(job_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO queue_eligibility (client_id, job_id) SELECT unnest($2::text[]), \
                     $1",
                )
                .bind(job_id.as_str())
                .bind(&clients)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE queue_jobs SET needs_reindex = false WHERE job_id = $1")
                    .bind(job_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(true)
            })
        })
        .await?)
    }

    async fn flag_for_reindex(&self, jobs: &[JobId]) -> anyhow::Result<()> {
        let jobs: Vec<&str> = jobs.iter().map(JobId::as_str).collect();
        self.db
            .execute("flag_for_reindex", || {
                sqlx::query("UPDATE queue_jobs SET needs_reindex = true WHERE job_id = ANY($1)")
                    .bind(jobs.clone())
            })
            .await?;
        Ok(())
    }

    async fn jobs_awaiting_index(&self, jobs: &[JobId]) -> anyhow::Result<HashSet<JobId>> {
        let jobs: Vec<&str> = jobs.iter().map(JobId::as_str).collect();
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("jobs_awaiting_index", || {
                sqlx::query_as(
                    "SELECT job_id FROM queue_jobs WHERE job_id = ANY($1) AND needs_reindex",
                )
                .bind(jobs.clone())
            })
            .await?;
        Ok(rows.into_iter().filter_map(|(j,)| valid_job(&j)).collect())
    }

    async fn read_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("read_workers", || {
                sqlx::query_as("SELECT client_id FROM queue_workers")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c,)| valid_client(&c))
            .collect())
    }

    async fn write_workers(&self, workers: &HashSet<ClientId>) -> anyhow::Result<()> {
        let workers: Vec<&str> = workers.iter().map(ClientId::as_str).collect();
        retry("write_workers", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                sqlx::query("DELETE FROM queue_workers")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO queue_workers (client_id) SELECT unnest($1::text[])")
                    .bind(&workers)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await
            })
        })
        .await?;
        Ok(())
    }

    async fn delete_worker(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_worker", || {
                sqlx::query("DELETE FROM queue_workers WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn stamp_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        // Hold the parent row through the upsert so deletion either cascades
        // this stamp or wins first and makes the stamp a no-op.
        self.db
            .execute("stamp_liveness", || {
                sqlx::query(
                    "INSERT INTO client_liveness (client_id, last_seen_at) \
                     SELECT client_id, now() FROM clients WHERE client_id = $1 FOR KEY SHARE ON \
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
}

#[cfg(test)]
mod tests {
    use futures::stream::{self, StreamExt, TryStreamExt};
    use rstest::rstest;

    use pipette_store::ClientStore;

    use super::*;
    use crate::scratch::TestDb;
    use crate::testing::{RACERS, a_client, public_key, race, test_db_or_skip};

    fn now() -> anyhow::Result<DateTime<Utc>> {
        Ok("2026-11-01T08:30:00.250Z".parse()?)
    }

    async fn enqueue(store: &SqlTodoStore, job: &JobId) -> anyhow::Result<()> {
        store
            .write_tmp(job, &serde_json::json!({"job": job}))
            .await?;
        store.promote_avail(job, ExpiresAt::Never, None).await
    }

    #[tokio::test]
    async fn body_reads_count_raw_rows_and_are_shared_by_clones() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let clone = store.clone();
        let corrupt = JobId::try_new("a-corrupt")?;
        let healthy = JobId::try_new("z-healthy")?;
        let missing = JobId::try_new("missing")?;
        stream::iter([&corrupt, &healthy])
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|job| enqueue(&store, job))
            .await?;
        sqlx::query("UPDATE queue_jobs SET body = 'not json' WHERE job_id = $1")
            .bind(corrupt.as_str())
            .execute(test_db.db().pool())
            .await?;
        assert!(store.get_avail_by_job(&missing).await?.is_none());
        assert_eq!(store.queue_body_reads(), 0);
        assert!(store.get_avail(&healthy, ExpiresAt::Never).await?.is_some());
        assert!(clone.get_avail_by_job(&healthy).await?.is_some());
        assert_eq!(store.queue_body_reads(), 2);
        let jobs = [corrupt.clone(), healthy.clone()];
        let batch = store.jobs_needing_reindex(&jobs).await?;
        assert_eq!(batch.unreadable, vec![corrupt.clone()]);
        assert_eq!(batch.jobs.len(), 1);
        assert_eq!(
            store.queue_body_reads(),
            4,
            "both batch rows were fetched, the corrupt one included"
        );
        assert!(store.get_avail_by_job(&corrupt).await.is_err());
        assert_eq!(store.queue_body_reads(), 5);
        store.jobs_awaiting_index(&jobs).await?;
        store.avail_clause_groups(&jobs).await?;
        assert_eq!(
            store.queue_body_reads(),
            5,
            "metadata and matcher projections do not read bodies"
        );
        let client = ClientId::try_new("reader")?;
        let until = now()? + chrono::Duration::hours(1);
        store
            .claim_job(&healthy, ExpiresAt::Never, &client, until)
            .await?;
        assert!(
            clone
                .get_leased(&healthy, ExpiresAt::Never, &client, until)
                .await?
                .is_some()
        );
        assert!(store.get_avail_by_job(&healthy).await?.is_none());
        assert_eq!(store.queue_body_reads(), 7);
        assert_eq!(clone.queue_body_reads(), 7);
        Ok(())
    }

    #[rstest]
    #[case::absent(serde_json::json!({"spec": {"large": "ignored"}}))]
    #[case::nulls(serde_json::json!({"requires": null, "clients": null, "any_of": null}))]
    #[case::clients_only(serde_json::json!({"clients": ["client-a"]}))]
    #[case::clauses(serde_json::json!({"requires": ["chip:chip-a"], "any_of": [["tag:lab"]], "excludes": ["os:ios"], "not_all_of": [["tag:other"]], "exclude_clients": ["client-b"], "spec": "ignored"}))]
    #[case::malformed(serde_json::json!({"requires": "chip:chip-a", "any_of": [null]}))]
    #[case::ignored_nul(serde_json::json!({"requires": ["chip:chip-a"], "spec": {"extra": "\u{0000}"}}))]
    #[case::ignored_nested_tokens(serde_json::json!({"requires": ["chip:chip-a"], "spec": {"nested": [{"requires": ["ignored"], "punctuation": "}:, [\"requires\"]"}], "extra": "\u{0000}"}}))]
    #[case::array(serde_json::json!([]))]
    #[tokio::test]
    async fn stored_clauses_preserve_matching(
        #[case] body: serde_json::Value,
    ) -> anyhow::Result<()> {
        use pipette_core::matching::{FleetTags, job_matches_client};
        use pipette_plan_types::{
            ContactEmail, DeviceProfile, NonEmptyTrimmedString, PublicKeyHex,
        };
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("projection-job")?;
        store.write_tmp(&job, &body).await?;
        store.promote_avail(&job, ExpiresAt::Never, None).await?;
        assert_eq!(
            store.get_avail(&job, ExpiresAt::Never).await?,
            Some(body.clone())
        );
        let groups = store
            .avail_clause_groups(std::slice::from_ref(&job))
            .await?;
        assert_eq!(groups.len(), 1);
        let (clauses, jobs) = &groups[0];
        assert_eq!(jobs, &vec![job]);
        assert!(clauses.get("spec").is_none());
        ["client-a", "client-b"].into_iter().try_for_each(|id| {
            let client = pipette_core::client::Client {
                client_id: ClientId::try_new(id)?,
                public_key: PublicKeyHex::try_new(
                    "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                )?,
                organization: NonEmptyTrimmedString::try_new("org")?,
                client_details: NonEmptyTrimmedString::try_new("details")?,
                contact_email: ContactEmail::try_new("a@b.com")?,
                status: pipette_core::client::ClientStatus::Approved,
                registered_at: now()?,
                device_profile: DeviceProfile {
                    device_chip_model: Some(NonEmptyTrimmedString::try_new("chip-a")?),
                    ..Default::default()
                },
                capabilities: Default::default(),
            };
            let tags = FleetTags::new(vec![(
                client.client_id.clone(),
                pipette_plan_types::Tag::try_new("lab")?,
            )]);
            assert_eq!(
                job_matches_client(&body, &client, &tags),
                job_matches_client(clauses, &client, &tags)
            );
            anyhow::Ok(())
        })?;
        Ok(())
    }

    #[rstest]
    #[case::ignored_value(r#"{"requires":["chip:chip-a"],"spec":{"extra":"\u0000"}}"#)]
    #[case::ignored_key(r#"{"\u0000":"ignored","requires":["chip:chip-a"]}"#)]
    #[case::escaped_field_name(r#"{"requ\u0069res":["chip:chip-a"],"spec":{"extra":"\u0000"}}"#)]
    #[case::duplicate_field(r#"{"requires":["\u0000"],"requires":["chip:chip-a"]}"#)]
    #[case::nested_field(
        r#"{"spec":{"requires":["ignored"],"extra":"\u0000"},"requires":["chip:chip-a"]}"#
    )]
    #[tokio::test]
    async fn the_projection_ignores_values_outside_the_matcher(
        #[case] body: &str,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let clauses: serde_json::Value = sqlx::query_scalar("SELECT queue_match_clauses($1)")
            .bind(body)
            .fetch_one(test_db.db().pool())
            .await?;
        assert_eq!(clauses, serde_json::json!({"requires": ["chip:chip-a"]}));
        Ok(())
    }

    #[rstest]
    #[case::invalid_json("{")]
    #[case::unrepresentable_matcher(r#"{"clients":["\u0000"]}"#)]
    #[case::out_of_range_matcher(r#"{"requires":1e999999}"#)]
    #[tokio::test]
    async fn an_unrepresentable_projection_does_not_block_the_queue(
        #[case] body: &str,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        sqlx::query(
            "INSERT INTO queue_jobs (job_id, body, revision) VALUES ('unrepresentable', $1, 1)",
        )
        .bind(body)
        .execute(test_db.db().pool())
        .await?;
        let clauses: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT match_clauses FROM queue_jobs WHERE job_id = 'unrepresentable'",
        )
        .fetch_one(test_db.db().pool())
        .await?;
        assert!(clauses.is_none());
        let keys: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT model_key, runtime_key FROM queue_jobs WHERE job_id = 'unrepresentable'",
        )
        .fetch_one(test_db.db().pool())
        .await?;
        assert_eq!(keys, (None, None));
        Ok(())
    }

    #[rstest]
    #[case::without_reindex_flag(false)]
    #[case::with_reindex_flag(true)]
    #[tokio::test]
    async fn failed_client_eligibility_cleanup_keeps_the_repair_triggers(
        #[case] pending: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_deleted")?;
        let job = JobId::try_new("cleanup-failure")?;
        store.write_tmp(&job, &serde_json::json!({})).await?;
        store.promote_avail(&job, ExpiresAt::Never, None).await?;
        store
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        store
            .write_workers(&HashSet::from([client.clone()]))
            .await?;
        if pending {
            store.write_pending_reindex(&client).await?;
        }
        sqlx::raw_sql(
            "CREATE FUNCTION fail_eligibility_delete() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected eligibility cleanup failure'; END; $$; \
             CREATE TRIGGER fail_eligibility_delete BEFORE DELETE ON queue_eligibility \
             FOR EACH STATEMENT EXECUTE FUNCTION fail_eligibility_delete();",
        )
        .execute(test_db.db().pool())
        .await?;

        pipette_store::purge_client_todo_state(&store, &client).await;
        assert_eq!(store.read_workers().await?, HashSet::from([client.clone()]));
        assert_eq!(store.has_pending_reindex(&client).await?, pending);
        assert_eq!(store.list_eligible_for_client(&client).await?.len(), 1);

        sqlx::query("DROP TRIGGER fail_eligibility_delete ON queue_eligibility")
            .execute(test_db.db().pool())
            .await?;
        pipette_store::purge_client_todo_state(&store, &client).await;
        assert!(store.read_workers().await?.is_empty());
        assert!(!store.has_pending_reindex(&client).await?);
        assert!(store.list_eligible_for_client(&client).await?.is_empty());
        Ok(())
    }

    #[rstest]
    #[case::compact(false)]
    #[case::pretty(true)]
    #[tokio::test]
    async fn projection_preserves_nested_and_escaped_matcher_values(
        #[case] pretty: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let expected = serde_json::json!({
            "requires": ["chip:chip-a"],
            "any_of": [["tag:lab", "tag:other"], []],
            "excludes": null,
            "exclude_clients": ["a\\b", "quotes\"and{}[]:,", "é"],
            "not_all_of": [["tag:busy"]],
            "clients": {"malformed": [1, true, false, null, {"clients": "nested"}]},
        });
        let mut body = expected.clone();
        body["spec"] = serde_json::json!({"payload": "\u{0000}", "clients": ["ignored"]});
        let encoded = if pretty {
            serde_json::to_string_pretty(&body)?
        } else {
            serde_json::to_string(&body)?
        };
        let actual: serde_json::Value = sqlx::query_scalar("SELECT queue_match_clauses($1)")
            .bind(encoded)
            .fetch_one(test_db.db().pool())
            .await?;
        assert_eq!(actual, expected);
        Ok(())
    }

    /// The tokenizer finds the end of each value when the text has multibyte
    /// characters or any JSON whitespace character.
    #[rstest]
    #[case::multibyte_values(
        r#"{"clients":["é","日本語","🦀"],"spec":{"extra":"\u0000"}}"#,
        serde_json::json!({"clients": ["é", "日本語", "🦀"]})
    )]
    #[case::multibyte_ignored_key(
        r#"{"ключ":"é","requires":["chip:é"]}"#,
        serde_json::json!({"requires": ["chip:é"]})
    )]
    #[case::mixed_whitespace(
        "{ \t\"clients\"\r\n:\t[ \"a\" ,\n\"b\"\t]\r\n,\"requires\" : [ ] }",
        serde_json::json!({"clients": ["a", "b"], "requires": []})
    )]
    #[case::whitespace_inside_strings(
        r#"{"clients":[" a ","b\t c","\"  \""]}"#,
        serde_json::json!({"clients": [" a ", "b\t c", "\"  \""]})
    )]
    #[case::scalar_value_before_close(
        "{\"excludes\":  null  \n}",
        serde_json::json!({"excludes": null})
    )]
    #[case::number_value_before_comma(
        r#"{"any_of":-15 ,"requires":"é"}"#,
        serde_json::json!({"any_of": -15, "requires": "é"})
    )]
    #[tokio::test]
    async fn projection_handles_unicode_and_whitespace(
        #[case] body: &str,
        #[case] expected: serde_json::Value,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let clauses: serde_json::Value = sqlx::query_scalar("SELECT queue_match_clauses($1)")
            .bind(body)
            .fetch_one(test_db.db().pool())
            .await?;
        assert_eq!(clauses, expected);
        Ok(())
    }

    #[tokio::test]
    async fn stored_clauses_accept_a_long_client_roster() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("long-roster")?;
        let clients: Vec<String> = (0..1000)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let body = serde_json::json!({"clients": clients});
        store.write_tmp(&job, &body).await?;
        store.promote_avail(&job, ExpiresAt::Never, None).await?;
        assert_eq!(
            store
                .avail_clause_groups(std::slice::from_ref(&job))
                .await?,
            vec![(body, vec![job])]
        );
        Ok(())
    }

    #[tokio::test]
    async fn clause_groups_omit_leases_and_batch_rows_are_idempotent() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let a = JobId::try_new("group-a")?;
        let b = JobId::try_new("group-b")?;
        let leased = JobId::try_new("group-leased")?;
        let client = ClientId::try_new("group-client")?;
        stream::iter([&a, &b, &leased])
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|job| enqueue(&store, job))
            .await?;
        store
            .claim_job(
                &leased,
                ExpiresAt::Never,
                &client,
                now()? + chrono::Duration::hours(1),
            )
            .await?;
        let groups = store
            .avail_clause_groups(&[a.clone(), b.clone(), leased])
            .await?;
        assert_eq!(
            groups,
            vec![(serde_json::json!({}), vec![a.clone(), b.clone()])]
        );
        stream::iter(0..2)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|_| async {
                store
                    .write_eligible_rows(&client, &[a.clone(), b.clone()])
                    .await
            })
            .await?;
        assert_eq!(store.list_eligible_for_client(&client).await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_denial_flags_the_job_and_invalidates_an_older_index_read() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("denial-race")?;
        let client = ClientId::try_new("denier")?;
        enqueue(&store, &job).await?;
        let snapshot = store
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs
            .remove(0);
        store
            .write_denied(&job, &client, &DenialRecord::default())
            .await?;
        assert!(
            !store
                .replace_eligibility(&job, snapshot.revision, &[])
                .await?
        );
        let snapshot = store
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs
            .remove(0);
        assert!(
            store
                .replace_eligibility(&job, snapshot.revision, &[])
                .await?
        );
        assert!(
            store
                .jobs_awaiting_index(std::slice::from_ref(&job))
                .await?
                .is_empty()
        );
        // A later denial must set the flag again after a successful index update.
        store
            .write_denied(&job, &client, &DenialRecord::default())
            .await?;
        assert_eq!(
            store
                .jobs_awaiting_index(std::slice::from_ref(&job))
                .await?,
            HashSet::from([job])
        );
        Ok(())
    }

    /// The state a job is in when the backlog counts it.
    #[derive(Clone, Copy, Debug)]
    enum Seeded {
        Available,
        NoDeadline,
        Expired,
        Denied,
        LeasedElsewhere,
        /// Ranked after a smaller run of its family that the client may not claim.
        BehindASmallerRun,
    }

    /// Queue `job` for `client` in `state`. The deadlines are relative to the
    /// database clock, which the backlog query reads.
    async fn seed(
        test_db: &TestDb,
        store: &SqlTodoStore,
        client: &ClientId,
        job: &JobId,
        state: Seeded,
    ) -> anyhow::Result<()> {
        let at =
            |minutes| ExpiresAt::At(to_second(Utc::now() + chrono::Duration::minutes(minutes)));
        let deadline = match state {
            Seeded::NoDeadline => ExpiresAt::Never,
            Seeded::Expired => at(-5),
            Seeded::Available
            | Seeded::Denied
            | Seeded::LeasedElsewhere
            | Seeded::BehindASmallerRun => at(60),
        };
        let sizes: &[i64] = &[512];
        let family = matches!(state, Seeded::BehindASmallerRun).then_some(("family", sizes));
        store.write_tmp(job, &serde_json::json!({})).await?;
        store.promote_avail(job, deadline, family).await?;
        store.write_eligible(client, job, deadline).await?;
        match state {
            Seeded::Denied => {
                store
                    .write_denied(job, client, &DenialRecord::default())
                    .await?
            }
            Seeded::LeasedElsewhere => {
                sqlx::query(
                    "UPDATE queue_jobs SET leased_by = 'ev1_other', lease_expiry = now() + \
                     interval '1 hour' WHERE job_id = $1",
                )
                .bind(job.as_str())
                .execute(test_db.db().pool())
                .await?;
            }
            Seeded::BehindASmallerRun => {
                let smaller = JobId::try_new("smaller-run")?;
                store.write_tmp(&smaller, &serde_json::json!({})).await?;
                store
                    .promote_avail(&smaller, deadline, Some(("family", [256].as_slice())))
                    .await?;
            }
            Seeded::Available | Seeded::NoDeadline | Seeded::Expired => {}
        }
        Ok(())
    }

    /// A job counts when a claim would accept it. A client with no claimable
    /// job has no entry.
    #[rstest]
    #[case::available(Seeded::Available, Some(1))]
    #[case::no_deadline(Seeded::NoDeadline, Some(1))]
    #[case::expired(Seeded::Expired, None)]
    #[case::denied(Seeded::Denied, None)]
    #[case::leased_elsewhere(Seeded::LeasedElsewhere, None)]
    #[case::behind_a_smaller_run(Seeded::BehindASmallerRun, Some(1))]
    #[tokio::test]
    async fn the_backlog_counts_the_claimable_jobs(
        #[case] state: Seeded,
        #[case] expected: Option<u64>,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_backlog")?;
        seed(
            &test_db,
            &store,
            &client,
            &JobId::try_new("backlog-job")?,
            state,
        )
        .await?;

        let backlog = store.backlog().await?;
        assert_eq!(backlog.get(&client).copied(), expected);
        assert_eq!(backlog.len(), usize::from(expected.is_some()));
        assert_eq!(
            store
                .campaign_backlog(&client, now()?)
                .await?
                .outside_campaigns,
            expected.unwrap_or(0)
        );
        Ok(())
    }

    /// A job counts once in each campaign that holds it, and in
    /// `outside_campaigns` when no campaign holds it. A cancelled plan, a plan
    /// outside a campaign and an expired membership hold no job.
    #[tokio::test]
    async fn the_campaign_backlog_counts_each_holding_campaign_once() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_campaigns")?;
        let campaign = |n: u8| format!("campaign_00000000-0000-0000-0000-00000000000{n}");
        let past = ExpiresAt::At(to_second(Utc::now() - chrono::Duration::minutes(5))).to_string();
        // (plan, status, campaign, cancel marker)
        let plans: [(&str, &str, Option<String>, bool); 7] = [
            ("plan-a", "active", Some(campaign(1)), false),
            ("plan-b", "active", Some(campaign(1)), false),
            ("plan-c", "active", Some(campaign(2)), false),
            ("plan-d", "cancelling", Some(campaign(3)), false),
            ("plan-e", "active", None, false),
            ("plan-f", "active", Some(campaign(4)), false),
            ("plan-g", "active", Some(campaign(5)), true),
        ];
        // (job, plan memberships, membership deadline)
        let jobs: [(&str, &[&str], &str); 7] = [
            ("job-two-plans", &["plan-a", "plan-b"], "never"),
            ("job-two-campaigns", &["plan-a", "plan-c"], "never"),
            ("job-no-plan", &[], "never"),
            ("job-cancelling", &["plan-d"], "never"),
            ("job-no-campaign", &["plan-e"], "never"),
            ("job-expired-membership", &["plan-f"], &past),
            ("job-cancel-marker", &["plan-g"], "never"),
        ];
        for (plan, status, campaign, cancelled) in &plans {
            sqlx::query(
                "INSERT INTO plans (plan_id, status, campaign_id, body) VALUES ($1, $2, $3, '{}')",
            )
            .bind(plan)
            .bind(status)
            .bind(campaign)
            .execute(test_db.db().pool())
            .await?;
            if *cancelled {
                sqlx::query("INSERT INTO cancelled_plans (plan_id) VALUES ($1)")
                    .bind(plan)
                    .execute(test_db.db().pool())
                    .await?;
            }
        }
        for (job, members, deadline) in &jobs {
            let job = JobId::try_new(*job)?;
            seed(&test_db, &store, &client, &job, Seeded::NoDeadline).await?;
            for plan in *members {
                sqlx::query(
                    "INSERT INTO job_plans (job_id, plan_id, expires_at, written_at) VALUES \
                     ($1, $2, $3, now())",
                )
                .bind(job.as_str())
                .bind(plan)
                .bind(deadline)
                .execute(test_db.db().pool())
                .await?;
            }
        }

        assert_eq!(
            store.campaign_backlog(&client, now()?).await?,
            CampaignBacklog {
                by_campaign: [
                    (CampaignId::try_new(campaign(1))?, 2),
                    (CampaignId::try_new(campaign(2))?, 1),
                ]
                .into_iter()
                .collect(),
                outside_campaigns: 5,
            }
        );
        assert_eq!(
            store
                .campaign_backlog(&ClientId::try_new("ev1_idle")?, now()?)
                .await?,
            CampaignBacklog::default()
        );
        Ok(())
    }

    #[rstest]
    #[case::fresh_creating(PlanStatus::Creating, Some("2026-11-01T08:30:01Z"), true)]
    #[case::at_cutoff(PlanStatus::Creating, Some("2026-11-01T08:30:00.250Z"), false)]
    #[case::stranded(PlanStatus::Creating, Some("2026-11-01T08:30:00Z"), false)]
    #[case::future(PlanStatus::Creating, Some("2027-01-01T00:00:00Z"), true)]
    #[case::old_active(PlanStatus::Active, Some("2026-11-01T08:30:00Z"), true)]
    #[case::old_complete(PlanStatus::Complete, Some("2026-11-01T08:30:00Z"), true)]
    #[case::invalid_date(PlanStatus::Creating, Some("invalid"), true)]
    #[case::missing_date(PlanStatus::Creating, None, true)]
    #[tokio::test]
    async fn campaign_backlog_excludes_only_stranded_creating_plans(
        #[case] status: PlanStatus,
        #[case] created_at: Option<&str>,
        #[case] counted: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_creating")?;
        let candidate = CampaignId::try_new("campaign_00000000-0000-0000-0000-000000000001")?;
        let healthy = CampaignId::try_new("campaign_00000000-0000-0000-0000-000000000002")?;
        stream::iter([
            ("candidate", status.label(), &candidate),
            ("healthy", PlanStatus::Active.label(), &healthy),
        ])
        .map(Ok::<_, anyhow::Error>)
        .try_for_each(|(plan, status, campaign)| {
            let test_db = &test_db;
            async move {
                sqlx::query(
                    "INSERT INTO plans (plan_id, status, campaign_id, body) VALUES ($1, $2, $3, $4)",
                )
                .bind(plan)
                .bind(status)
                .bind(campaign.as_str())
                .bind(serde_json::json!({"created_at": created_at}).to_string())
                .execute(test_db.db().pool())
                .await?;
                anyhow::Ok(())
            }
        })
        .await?;
        stream::iter(["shared", "candidate-only"])
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|name| {
                let test_db = &test_db;
                let store = &store;
                let client = &client;
                async move {
                    let job = JobId::try_new(name)?;
                    seed(test_db, store, client, &job, Seeded::NoDeadline).await?;
                    sqlx::query(
                        "INSERT INTO job_plans (job_id, plan_id, expires_at, written_at) \
                         VALUES ($1, 'candidate', 'never', now())",
                    )
                    .bind(name)
                    .execute(test_db.db().pool())
                    .await?;
                    anyhow::Ok(())
                }
            })
            .await?;
        sqlx::query(
            "INSERT INTO job_plans (job_id, plan_id, expires_at, written_at) \
             VALUES ('shared', 'healthy', 'never', now())",
        )
        .execute(test_db.db().pool())
        .await?;

        let backlog = store.campaign_backlog(&client, now()?).await?;
        assert_eq!(backlog.by_campaign.get(&healthy), Some(&1));
        assert_eq!(backlog.by_campaign.get(&candidate), counted.then_some(&2));
        assert_eq!(backlog.outside_campaigns, u64::from(!counted));
        assert_eq!(store.backlog().await?.get(&client), Some(&2));
        Ok(())
    }

    /// A row carries no deadline of its own: each read takes the job's.
    #[tokio::test]
    async fn an_eligibility_row_reads_its_jobs_deadline() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("redated_client")?;
        let job = JobId::try_new("redated-job")?;
        let old = ExpiresAt::At(to_second(now()?));
        store.write_tmp(&job, &serde_json::json!({})).await?;
        store.promote_avail(&job, old, None).await?;
        store.write_eligible(&client, &job, old).await?;
        store.redate_avail(&job, old, ExpiresAt::Never).await?;

        assert_eq!(
            store.list_all_eligible().await?,
            vec![(client.clone(), job.clone(), ExpiresAt::Never)]
        );
        assert_eq!(
            store.list_eligible_for_client(&client).await?,
            vec![(job.clone(), ExpiresAt::Never)]
        );
        store.delete_eligible(&client, &job, old).await?;
        assert!(store.list_all_eligible().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_reindex_read_decodes_only_the_requested_jobs() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let healthy = JobId::try_new("healthy-job")?;
        let corrupt = JobId::try_new("corrupt-job")?;
        let missing = JobId::try_new("missing-job")?;
        enqueue(&store, &healthy).await?;
        enqueue(&store, &corrupt).await?;
        sqlx::query("UPDATE queue_jobs SET body = 'not json' WHERE job_id = $1")
            .bind(corrupt.as_str())
            .execute(test_db.db().pool())
            .await?;

        assert!(store.jobs_needing_reindex(&[]).await?.jobs.is_empty());
        let jobs = store
            .jobs_needing_reindex(&[healthy.clone(), healthy.clone(), missing])
            .await?
            .jobs;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, healthy);
        assert_eq!(jobs[0].body, serde_json::json!({"job": healthy}));
        let batch = store
            .jobs_needing_reindex(&[corrupt.clone(), healthy.clone()])
            .await?;
        assert_eq!(batch.unreadable, vec![corrupt]);
        assert_eq!(batch.jobs.len(), 1, "a corrupt row does not fail the batch");
        Ok(())
    }

    /// Clear each requested job's flag, as an index pass with no matching client does.
    async fn index_all(store: &SqlTodoStore, jobs: &[JobId]) -> anyhow::Result<()> {
        stream::iter(store.jobs_needing_reindex(jobs).await?.jobs)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|job| async move {
                store
                    .replace_eligibility(&job.job_id, job.revision, &[])
                    .await?;
                anyhow::Ok(())
            })
            .await?;
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum BackInQueue {
        Inserted,
        Recycled,
        Redated,
    }

    /// Each way a job enters the available set raises its reindex flag.
    #[rstest]
    #[case::inserted(BackInQueue::Inserted)]
    #[case::recycled(BackInQueue::Recycled)]
    #[case::redated(BackInQueue::Redated)]
    #[tokio::test]
    async fn a_job_back_in_the_queue_needs_reindex(#[case] how: BackInQueue) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("flagged-job")?;
        let client = ClientId::try_new("flag_holder")?;
        let lease_expiry = to_second(now()?);
        let dated = ExpiresAt::At(to_second(now()?));
        store.write_tmp(&job, &serde_json::json!({})).await?;
        store.promote_avail(&job, dated, None).await?;
        if !matches!(how, BackInQueue::Inserted) {
            index_all(&store, std::slice::from_ref(&job)).await?;
            assert!(
                store
                    .jobs_awaiting_index(std::slice::from_ref(&job))
                    .await?
                    .is_empty()
            );
        }
        match how {
            BackInQueue::Inserted => {}
            BackInQueue::Recycled => {
                store.claim_job(&job, dated, &client, lease_expiry).await?;
                store
                    .recycle_lease(&job, dated, &client, lease_expiry)
                    .await?;
            }
            BackInQueue::Redated => {
                store.redate_avail(&job, dated, ExpiresAt::Never).await?;
            }
        }
        let flagged: Vec<JobId> = store
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs
            .into_iter()
            .map(|flagged| flagged.job_id)
            .collect();
        assert_eq!(flagged, vec![job]);
        Ok(())
    }

    /// A claim between the read and the write moves the revision. The write
    /// then changes nothing. The next run indexes the job after it returns.
    #[tokio::test]
    async fn a_stale_revision_keeps_the_rows_and_the_flag() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("moved-job")?;
        let (before, after) = (ClientId::try_new("before")?, ClientId::try_new("after")?);
        let holder = ClientId::try_new("holder")?;
        let lease_expiry = to_second(now()?);
        enqueue(&store, &job).await?;
        store
            .write_eligible(&before, &job, ExpiresAt::Never)
            .await?;
        let read = store
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs;
        let [flagged] = read.as_slice() else {
            anyhow::bail!("one flagged job, got {read:?}");
        };

        store
            .claim_job(&job, ExpiresAt::Never, &holder, lease_expiry)
            .await?;
        let replaced = store
            .replace_eligibility(&job, flagged.revision, std::slice::from_ref(&after))
            .await?;
        assert!(!replaced);
        assert_eq!(
            store.list_eligible_for_client(&before).await?,
            vec![(job.clone(), ExpiresAt::Never)]
        );
        assert!(
            store
                .jobs_needing_reindex(std::slice::from_ref(&job))
                .await?
                .jobs
                .is_empty(),
            "a leased job waits"
        );
        assert_eq!(
            store
                .jobs_awaiting_index(std::slice::from_ref(&job))
                .await?,
            HashSet::from([job.clone()])
        );

        store
            .recycle_lease(&job, ExpiresAt::Never, &holder, lease_expiry)
            .await?;
        let read = store
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs;
        let [flagged] = read.as_slice() else {
            anyhow::bail!("one flagged job, got {read:?}");
        };
        assert!(
            store
                .replace_eligibility(&job, flagged.revision, std::slice::from_ref(&after))
                .await?
        );
        assert!(store.list_eligible_for_client(&before).await?.is_empty());
        assert_eq!(
            store.list_eligible_for_client(&after).await?,
            vec![(job.clone(), ExpiresAt::Never)]
        );
        assert!(store.jobs_awaiting_index(&[job]).await?.is_empty());
        Ok(())
    }

    /// A write replaces the whole set.
    #[tokio::test]
    async fn the_worker_set_round_trips() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        assert!(store.read_workers().await?.is_empty());
        for names in [&["ev1_a", "ev1_b"][..], &["ev1_c"][..]] {
            let workers = names
                .iter()
                .map(|name| ClientId::try_new(*name))
                .collect::<Result<HashSet<_>, _>>()?;
            store.write_workers(&workers).await?;
            assert_eq!(store.read_workers().await?, workers);
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_denial_code_keeps_its_parse_error_and_column_context() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("invalid-denial-code")?;
        let client = ClientId::try_new("ev1_denied")?;
        enqueue(&store, &job).await?;
        sqlx::query(
            "INSERT INTO queue_denials (job_id, client_id, failure_code) \
             VALUES ($1, $2, 'unknown_code')",
        )
        .bind(job.as_str())
        .bind(client.as_str())
        .execute(test_db.db().pool())
        .await?;

        let error = store
            .get_denied(&job, &client)
            .await
            .err()
            .context("an invalid denial code must be rejected")?;
        assert_eq!(
            error.to_string(),
            "queue_denials.failure_code \"unknown_code\""
        );
        assert!(error.source().is_some());
        Ok(())
    }

    async fn register_client(test_db: &TestDb, id: ClientId) -> anyhow::Result<()> {
        crate::clients::SqlClientStore::new(test_db.db().clone())
            .put_client(&a_client(id, public_key(0x01)?)?)
            .await
    }

    #[rstest]
    #[case::new(false)]
    #[case::existing(true)]
    #[tokio::test]
    async fn a_liveness_write_racing_client_deletion_leaves_no_row(
        #[case] existing: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let clients = crate::clients::SqlClientStore::new(test_db.db().clone());
        let ids = (0..RACERS / 2)
            .map(|pair| ClientId::try_new(format!("liveness_race_{pair}")))
            .collect::<Result<Vec<_>, _>>()?;
        stream::iter(ids.iter())
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|id| {
                let test_db = &test_db;
                let store = &store;
                async move {
                    register_client(test_db, id.clone()).await?;
                    if existing {
                        store.stamp_liveness(id).await?;
                    }
                    anyhow::Ok(())
                }
            })
            .await?;

        race(|n| {
            let store = store.clone();
            let clients = clients.clone();
            let id = ids[n / 2].clone();
            async move {
                if n % 2 == 0 {
                    clients.delete_client(&id).await
                } else {
                    store.stamp_liveness(&id).await
                }
            }
        })
        .await?;
        assert!(store.list_liveness().await?.is_empty());

        // A request authenticated before deletion is a no-op.
        store.stamp_liveness(&ids[0]).await?;
        assert!(store.list_liveness().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn sql_todo_store_passes_the_todo_contract() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        register_client(&test_db, ClientId::try_new("live_c1")?).await?;
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
        assert_eq!(store.list_leases().await?.len(), 1);
        assert!(store.list_avail_entries().await?.is_empty());
        Ok(())
    }

    /// Two racers per client, one tier of jobs that every client may run.
    /// A failure means a claim exhausted its retries, two clients acquired the same job,
    /// or one client acquired two jobs.
    #[tokio::test]
    async fn racing_claimers_win_distinct_jobs_and_one_lease_each() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let clients = (0..RACERS / 2)
            .map(|n| ClientId::try_new(format!("racer_{n}")))
            .collect::<Result<Vec<_>, _>>()?;
        for n in 0..RACERS {
            let job = JobId::try_new(format!("race-job-{n}"))?;
            enqueue(&store, &job).await?;
            for client in &clients {
                store.write_eligible(client, &job, ExpiresAt::Never).await?;
            }
        }
        let at = now()?;
        let lease_expiry = at + chrono::Duration::hours(1);
        let outcomes = race(|n| {
            let store = store.clone();
            let client = clients[n % clients.len()].clone();
            async move {
                store
                    .claim_for_client(&client, lease_expiry, 0, &[], None, None, at)
                    .await
            }
        })
        .await?;

        let granted: HashSet<JobId> = outcomes
            .iter()
            .filter_map(|outcome| {
                if let ClaimOutcome::Granted { lease, .. } = outcome {
                    Some(lease.job_id.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(granted.len(), clients.len(), "{outcomes:?}");
        for client in &clients {
            assert_eq!(store.leases_for_client(client).await?.len(), 1, "{client}");
        }
        Ok(())
    }

    #[rstest]
    #[case::claim(false)]
    #[case::reclaim(true)]
    #[tokio::test]
    async fn client_transaction_settles_an_unrecycled_lease(
        #[case] reclaim: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("lapsed-client")?;
        let old = JobId::try_new("old-job")?;
        let next = JobId::try_new("next-job")?;
        let at = now()?;
        let lapsed_at = at - chrono::Duration::seconds(1);
        enqueue(&store, &old).await?;
        store
            .claim_job(&old, ExpiresAt::Never, &client, lapsed_at)
            .await?;
        enqueue(&store, &next).await?;
        store
            .write_eligible(&client, &next, ExpiresAt::Never)
            .await?;
        let until = at + chrono::Duration::minutes(5);
        if reclaim {
            assert!(matches!(
                store
                    .reclaim_for_client(&next, &client, until, 3, at)
                    .await?,
                ReclaimOutcome::Granted { .. }
            ));
        } else {
            assert!(matches!(
                store
                    .claim_for_client(&client, until, 3, &[], None, None, at)
                    .await?,
                ClaimOutcome::Granted { .. }
            ));
        }
        let leases = store.leases_for_client(&client).await?;
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].job_id, next);
        assert!(store.get_avail_by_job(&old).await?.is_some());
        assert_eq!(
            store.list_lost_leases(&old).await?,
            [(client, to_second(lapsed_at))]
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_claim_rolls_back_lapsed_lease_settlement() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("rollback-client")?;
        let old = JobId::try_new("old-job")?;
        let next = JobId::try_new("invalid-job")?;
        let at = now()?;
        enqueue(&store, &old).await?;
        store.claim_job(&old, ExpiresAt::Never, &client, at).await?;
        enqueue(&store, &next).await?;
        store
            .write_eligible(&client, &next, ExpiresAt::Never)
            .await?;
        sqlx::query("UPDATE queue_jobs SET body = 'not JSON' WHERE job_id = $1")
            .bind(next.as_str())
            .execute(test_db.db().pool())
            .await?;
        assert!(
            store
                .claim_for_client(
                    &client,
                    at + chrono::Duration::minutes(5),
                    3,
                    &[],
                    None,
                    None,
                    at
                )
                .await
                .is_err()
        );
        assert!(store.get_avail_by_job(&old).await?.is_none());
        assert!(store.list_lost_leases(&old).await?.is_empty());
        let leases = store.leases_for_client(&client).await?;
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].job_id, old);
        Ok(())
    }

    #[tokio::test]
    async fn a_hand_back_counts_only_the_lease_the_caller_checked() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("checked-client")?;
        let job = JobId::try_new("checked-job")?;
        let at = now()?;
        let until = at + chrono::Duration::minutes(5);
        enqueue(&store, &job).await?;
        store
            .claim_job(&job, ExpiresAt::Never, &client, until)
            .await?;
        let result = store
            .claim_for_client(&client, until, 2, &[], None, None, at)
            .await?;
        let ClaimOutcome::CheckHeldLease { lease, .. } = result else {
            anyhow::bail!("a held lease needs an access check before loss accounting");
        };
        assert!(store.list_lost_leases(&job).await?.is_empty());
        let result = store
            .claim_for_client(&client, until, 2, &[], Some(&lease), None, at)
            .await?;
        assert!(matches!(result, ClaimOutcome::HandedBack { .. }));
        assert_eq!(store.list_lost_leases(&job).await?.len(), 1);

        // A heartbeat changes the lease while its access check is in flight.
        let renewed = until + chrono::Duration::minutes(1);
        store.renew_lease(&job, &client, renewed).await?;
        let result = store
            .claim_for_client(&client, renewed, 2, &[], Some(&lease), None, at)
            .await?;
        let ClaimOutcome::CheckHeldLease { lease, .. } = result else {
            anyhow::bail!("the renewed lease must pass its own access check");
        };
        assert_eq!(store.list_lost_leases(&job).await?.len(), 1);
        let result = store
            .claim_for_client(&client, renewed, 2, &[], Some(&lease), None, at)
            .await?;
        assert!(matches!(result, ClaimOutcome::Withheld(_)));
        assert_eq!(store.list_lost_leases(&job).await?.len(), 2);
        Ok(())
    }

    #[rstest]
    #[case::recorded(true, false, 3, false, 0)]
    #[case::pending_repair(false, true, 3, true, 0)]
    #[case::last_allowed_loss(false, false, 1, true, 1)]
    #[tokio::test]
    async fn terminal_lapsed_jobs_are_never_regranted(
        #[case] recorded: bool,
        #[case] pending: bool,
        #[case] limit: usize,
        #[case] needs_settlement: bool,
        #[case] losses: usize,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("terminal-client")?;
        let job = JobId::try_new("terminal-job")?;
        let at = now()?;
        enqueue(&store, &job).await?;
        store.claim_job(&job, ExpiresAt::Never, &client, at).await?;
        store
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        if recorded {
            sqlx::query(
                "INSERT INTO submission_outcomes (job_id, outcome) VALUES ($1, 'finished')",
            )
            .bind(job.as_str())
            .execute(test_db.db().pool())
            .await?;
        }
        if pending {
            sqlx::query("INSERT INTO submission_outcome_repairs (job_id, generation, pending) VALUES ($1, 1, true)")
                .bind(job.as_str()).execute(test_db.db().pool()).await?;
        }
        let result = store
            .claim_for_client(
                &client,
                at + chrono::Duration::minutes(5),
                limit,
                &[],
                None,
                None,
                at,
            )
            .await?;
        assert_eq!(
            matches!(result, ClaimOutcome::SettlementRequired),
            needs_settlement
        );
        assert!(matches!(
            result,
            ClaimOutcome::SettlementRequired | ClaimOutcome::Empty
        ));
        assert!(store.get_avail_by_job(&job).await?.is_none());
        assert_eq!(store.list_lost_leases(&job).await?.len(), losses);
        assert_eq!(
            !store.leases_for_client(&client).await?.is_empty(),
            needs_settlement
        );
        Ok(())
    }

    #[derive(Clone, Copy, Debug)]
    enum Rival {
        Sooner,
        Denied,
        Expired,
        PassedOver,
        LeasedElsewhere,
    }

    /// A rival job with the sooner deadline, against a plain job with none.
    #[rstest]
    #[case::the_sooner_deadline_wins(Rival::Sooner, "rival")]
    #[case::a_denied_job_is_skipped(Rival::Denied, "plain")]
    #[case::an_expired_job_is_skipped(Rival::Expired, "plain")]
    #[case::a_passed_over_job_is_skipped(Rival::PassedOver, "plain")]
    #[case::a_job_leased_elsewhere_is_skipped(Rival::LeasedElsewhere, "plain")]
    #[tokio::test]
    async fn the_claim_picks_a_free_job_by_deadline(
        #[case] rival_kind: Rival,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let at = now()?;
        let client = ClientId::try_new("picker")?;
        let rival = JobId::try_new("rival")?;
        let plain = JobId::try_new("plain")?;
        let rival_deadline = if matches!(rival_kind, Rival::Expired) {
            ExpiresAt::At(to_second(at) - chrono::Duration::hours(1))
        } else {
            ExpiresAt::At(to_second(at) + chrono::Duration::days(1))
        };
        for (job, deadline) in [(&rival, rival_deadline), (&plain, ExpiresAt::Never)] {
            store
                .write_tmp(job, &serde_json::json!({"job": job}))
                .await?;
            store.promote_avail(job, deadline, None).await?;
            store.write_eligible(&client, job, deadline).await?;
        }
        match rival_kind {
            Rival::Denied => {
                store
                    .write_denied(&rival, &client, &DenialRecord::default())
                    .await?;
            }
            Rival::LeasedElsewhere => {
                let other = ClientId::try_new("other")?;
                store
                    .claim_job(
                        &rival,
                        rival_deadline,
                        &other,
                        at + chrono::Duration::hours(1),
                    )
                    .await?;
            }
            Rival::Sooner | Rival::Expired | Rival::PassedOver => {}
        }
        let passed_over = if matches!(rival_kind, Rival::PassedOver) {
            vec![rival.clone()]
        } else {
            Vec::new()
        };

        let outcome = store
            .claim_for_client(
                &client,
                at + chrono::Duration::hours(1),
                0,
                &passed_over,
                None,
                None,
                at,
            )
            .await?;
        let ClaimOutcome::Granted { lease, .. } = outcome else {
            anyhow::bail!("expected a grant, got {outcome:?}");
        };
        assert_eq!(lease.job_id.as_str(), expected);
        Ok(())
    }

    /// The lease a claim of `client` grants, `None` when it grants nothing.
    async fn grant(
        store: &SqlTodoStore,
        client: &ClientId,
        passed_over: &[JobId],
    ) -> anyhow::Result<Option<Lease>> {
        let at = now()?;
        let outcome = store
            .claim_for_client(
                client,
                at + chrono::Duration::hours(1),
                0,
                passed_over,
                None,
                None,
                at,
            )
            .await?;
        if matches!(outcome, ClaimOutcome::Empty) {
            return Ok(None);
        }
        let ClaimOutcome::Granted { lease, .. } = outcome else {
            anyhow::bail!("expected a grant or nothing, got {outcome:?}");
        };
        Ok(Some(lease))
    }

    /// A family hands out its smaller runs first: the fewer smaller runs of its
    /// family the queue holds, leased or not, the sooner a run goes, and a job in
    /// no family ranks first. A device with nothing ranked sooner takes a larger
    /// run, so no device waits for the order.
    #[tokio::test]
    async fn a_family_hands_out_its_smaller_runs_first() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let clients = ["ev1_a", "ev1_b", "ev1_c", "ev1_d", "ev1_e"]
            .map(ClientId::try_new)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        // Prompt tokens and decode tokens.
        stream::iter([
            ("small", Some([256, 100])),
            ("longer-prompt", Some([512, 100])),
            ("longer-decode", Some([256, 200])),
            ("large", Some([512, 200])),
            ("solo", None),
        ])
        .map(Ok::<_, anyhow::Error>)
        .try_for_each(|(name, sizes)| {
            let (store, clients) = (&store, &clients);
            async move {
                let job = JobId::try_new(name)?;
                store.write_tmp(&job, &serde_json::json!({})).await?;
                let family = sizes.as_ref().map(|sizes| ("family", sizes.as_slice()));
                store.promote_avail(&job, ExpiresAt::Never, family).await?;
                stream::iter(clients)
                    .map(Ok::<_, anyhow::Error>)
                    .try_for_each(|client| store.write_eligible(client, &job, ExpiresAt::Never))
                    .await
            }
        })
        .await?;
        let solo = JobId::try_new("solo")?;

        let small = grant(&store, &clients[0], std::slice::from_ref(&solo))
            .await?
            .context("a grant")?;
        assert_eq!(small.job_id.as_str(), "small");
        let outside = grant(&store, &clients[1], &[]).await?.context("a grant")?;
        assert_eq!(outside.job_id, solo);
        // While the small run runs, the two runs that wait for it alone go out,
        // then the one that waits for all three.
        let first = grant(&store, &clients[2], &[]).await?.context("a grant")?;
        let second = grant(&store, &clients[3], &[]).await?.context("a grant")?;
        assert_eq!(
            HashSet::from([first.job_id.as_str(), second.job_id.as_str()]),
            HashSet::from(["longer-prompt", "longer-decode"])
        );
        let large = grant(&store, &clients[4], &[]).await?.context("a grant")?;
        assert_eq!(large.job_id.as_str(), "large");
        Ok(())
    }

    /// Only a run's own family ranks it: another family's first run goes before
    /// this family's second, however small this family's runs are.
    #[tokio::test]
    async fn another_familys_runs_do_not_rank_a_run() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_ranked")?;
        stream::iter([
            ("first", "family", 100, false),
            ("second", "family", 101, true),
            ("other", "other", 5000, true),
        ])
        .map(Ok::<_, anyhow::Error>)
        .try_for_each(|(name, family, size, eligible)| {
            let (store, client) = (&store, &client);
            async move {
                let job = JobId::try_new(name)?;
                store.write_tmp(&job, &serde_json::json!({})).await?;
                store
                    .promote_avail(&job, ExpiresAt::Never, Some((family, [size].as_slice())))
                    .await?;
                if eligible {
                    store.write_eligible(client, &job, ExpiresAt::Never).await?;
                }
                Ok(())
            }
        })
        .await?;
        let granted = grant(&store, &client, &[]).await?.context("a grant")?;
        assert_eq!(granted.job_id.as_str(), "other");
        Ok(())
    }

    /// A job's larger runs are the queued, unleased runs of its family that are
    /// no smaller in any size and larger in one. A run smaller in some size, a
    /// twin, a leased run and another family's run are none.
    #[tokio::test]
    async fn a_jobs_larger_runs_are_no_smaller_in_any_size() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        // Prompt tokens and decode tokens.
        stream::iter([
            ("failed", "family", [256, 100]),
            ("longer-prompt", "family", [512, 100]),
            ("longer-both", "family", [512, 200]),
            ("shorter-prompt-longer-decode", "family", [128, 200]),
            ("twin", "family", [256, 100]),
            ("leased", "family", [1024, 100]),
            ("other-family", "other", [512, 100]),
        ])
        .map(Ok::<_, anyhow::Error>)
        .try_for_each(|(name, family, sizes)| {
            let store = &store;
            async move {
                let job = JobId::try_new(name)?;
                store.write_tmp(&job, &serde_json::json!({})).await?;
                store
                    .promote_avail(&job, ExpiresAt::Never, Some((family, sizes.as_slice())))
                    .await
            }
        })
        .await?;
        let client = ClientId::try_new("ev1_runner")?;
        let until = now()? + chrono::Duration::hours(1);
        let leased = JobId::try_new("leased")?;
        store
            .claim_job(&leased, ExpiresAt::Never, &client, until)
            .await?;

        let larger: HashSet<String> = store
            .queued_larger_runs(&JobId::try_new("failed")?)
            .await?
            .into_iter()
            .map(|(avail, _)| avail.job_id.to_string())
            .collect();
        assert_eq!(
            larger,
            HashSet::from(["longer-prompt".to_owned(), "longer-both".to_owned()])
        );
        Ok(())
    }

    /// A job body whose download is `model` and `runtime`.
    fn artifact_body(job: &JobId, model: &str, runtime: &str) -> serde_json::Value {
        serde_json::json!({
            "job_id": job,
            "spec": {
                "model": {"type": "fixture", "name": model},
                "runtime": {"type": "fixture", "name": runtime},
            },
        })
    }

    fn descriptor(name: &str) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        Ok(serde_json::from_value(
            serde_json::json!({"name": name, "type": "fixture"}),
        )?)
    }

    async fn seed_artifact_job(
        store: &SqlTodoStore,
        client: &ClientId,
        job: &JobId,
        model: &str,
        runtime: &str,
        deadline: ExpiresAt,
    ) -> anyhow::Result<()> {
        store
            .write_tmp(job, &artifact_body(job, model, runtime))
            .await?;
        store.promote_avail(job, deadline, None).await?;
        store.write_eligible(client, job, deadline).await
    }

    #[derive(Clone, Copy, Debug)]
    enum PriorEnd {
        Finished,
        /// Queue maintenance recycles the lapsed lease, and the client then
        /// reclaims the job and finishes it.
        Reclaimed,
        Failed,
        /// A failure report for another job, which leaves the last grant.
        FailedElsewhere,
        LeaseLost,
    }

    #[derive(Clone, Copy, Debug)]
    enum Advert {
        Absent,
        Empty,
        ModelA,
        /// A descriptor that jsonb cannot represent.
        Unrepresentable,
    }

    /// The client runs each prior job in turn; the last one ends as `end`. The
    /// candidates are `model-job` (A, R9), `runtime-job` (Z, R1) and
    /// `fresh-job` (C, R3). An expected job of `None` means any job.
    #[rstest]
    #[case::the_held_model_wins(&[("A", "R1")], PriorEnd::Finished, Advert::Absent, false, ClaimAffinity::Model, Some("model-job"))]
    #[case::the_held_runtime_wins(&[("Y", "R1")], PriorEnd::Finished, Advert::Absent, false, ClaimAffinity::Runtime, Some("runtime-job"))]
    #[case::only_the_last_grant_counts(&[("A", "R5"), ("Y", "R1")], PriorEnd::Finished, Advert::Absent, false, ClaimAffinity::Runtime, Some("runtime-job"))]
    #[case::a_failure_forgets_the_grant(&[("A", "R1")], PriorEnd::Failed, Advert::Absent, false, ClaimAffinity::None, None)]
    #[case::a_failure_elsewhere_keeps_the_grant(&[("A", "R1")], PriorEnd::FailedElsewhere, Advert::Absent, false, ClaimAffinity::Model, Some("model-job"))]
    #[case::a_reclaim_restores_the_grant(&[("A", "R1")], PriorEnd::Reclaimed, Advert::Absent, false, ClaimAffinity::Model, Some("model-job"))]
    #[case::a_lost_lease_forgets_the_grant(&[("A", "R1")], PriorEnd::LeaseLost, Advert::Absent, false, ClaimAffinity::None, None)]
    #[case::an_advertised_set_replaces_the_grant(&[("Y", "R1")], PriorEnd::Finished, Advert::ModelA, false, ClaimAffinity::Model, Some("model-job"))]
    #[case::an_empty_advertised_set_holds_nothing(&[("A", "R1")], PriorEnd::Finished, Advert::Empty, false, ClaimAffinity::None, None)]
    #[case::an_unrepresentable_descriptor_matches_nothing(&[("A", "R1")], PriorEnd::Finished, Advert::Unrepresentable, false, ClaimAffinity::None, None)]
    #[case::a_sooner_deadline_beats_affinity(&[("A", "R1")], PriorEnd::Finished, Advert::Absent, true, ClaimAffinity::None, Some("fresh-job"))]
    #[tokio::test]
    async fn the_claim_prefers_the_artifacts_the_client_holds(
        #[case] prior: &[(&str, &str)],
        #[case] end: PriorEnd,
        #[case] advert: Advert,
        #[case] fresh_is_sooner: bool,
        #[case] expected_affinity: ClaimAffinity,
        #[case] expected_job: Option<&str>,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let at = now()?;
        let client = ClientId::try_new("picker")?;
        register_client(&test_db, client.clone()).await?;
        let lease_expiry = to_second(at) + chrono::Duration::hours(1);

        for (index, (model, runtime)) in prior.iter().enumerate() {
            let job = JobId::try_new(format!("prior-{index}"))?;
            seed_artifact_job(&store, &client, &job, model, runtime, ExpiresAt::Never).await?;
            let last = index + 1 == prior.len();
            // A lease that lapses before the next claim is a lost lease.
            let expiry = if last && matches!(end, PriorEnd::LeaseLost) {
                to_second(at) - chrono::Duration::minutes(1)
            } else {
                lease_expiry
            };
            let outcome = store
                .claim_for_client(&client, expiry, 0, &[], None, None, at)
                .await?;
            let ClaimOutcome::Granted { lease, .. } = outcome else {
                anyhow::bail!("expected a grant of {job}, got {outcome:?}");
            };
            anyhow::ensure!(lease.job_id == job, "granted {}, not {job}", lease.job_id);
            // An earlier prior job finishes, whatever `end` is.
            let end = if last { end } else { PriorEnd::Finished };
            let mut lease = lease;
            match end {
                PriorEnd::Finished => {}
                PriorEnd::Reclaimed => {
                    // What queue-maintenance does to a lapsed lease.
                    store
                        .write_lost_lease(&job, &client, lease.lease_expiry)
                        .await?;
                    store
                        .recycle_lease(&job, lease.expires_at, &client, lease.lease_expiry)
                        .await?;
                    let outcome = store
                        .reclaim_for_client(&job, &client, lease_expiry, 0, at)
                        .await?;
                    let ReclaimOutcome::Granted {
                        lease: reclaimed, ..
                    } = outcome
                    else {
                        anyhow::bail!("expected a reclaim grant of {job}, got {outcome:?}");
                    };
                    lease = reclaimed;
                }
                PriorEnd::Failed => store.forget_last_grant(&job, &client).await?,
                PriorEnd::FailedElsewhere => {
                    store
                        .forget_last_grant(&JobId::try_new("elsewhere")?, &client)
                        .await?;
                }
                PriorEnd::LeaseLost => continue,
            }
            store
                .delete_lease(&job, lease.expires_at, &client, lease.lease_expiry)
                .await?;
        }

        let sooner = ExpiresAt::At(to_second(at) + chrono::Duration::days(1));
        for (name, model, runtime) in [
            ("model-job", "A", "R9"),
            ("runtime-job", "Z", "R1"),
            ("fresh-job", "C", "R3"),
        ] {
            let job = JobId::try_new(name)?;
            let deadline = if fresh_is_sooner && name == "fresh-job" {
                sooner
            } else {
                ExpiresAt::Never
            };
            seed_artifact_job(&store, &client, &job, model, runtime, deadline).await?;
        }

        let cached = match advert {
            Advert::Absent => None,
            Advert::Empty => Some(CachedArtifacts::default()),
            Advert::ModelA => Some(CachedArtifacts {
                models: vec![descriptor("A")?],
                runtimes: Vec::new(),
            }),
            Advert::Unrepresentable => Some(CachedArtifacts {
                models: vec![descriptor("A\u{0}")?, descriptor("\u{0}")?],
                runtimes: vec![descriptor("\u{0}")?],
            }),
        };
        let outcome = store
            .claim_for_client(&client, lease_expiry, 0, &[], None, cached.as_ref(), at)
            .await?;
        let ClaimOutcome::Granted {
            lease, affinity, ..
        } = outcome
        else {
            anyhow::bail!("expected a grant, got {outcome:?}");
        };
        assert_eq!(affinity, expected_affinity);
        if let Some(expected) = expected_job {
            assert_eq!(lease.job_id.as_str(), expected);
        }
        Ok(())
    }

    /// With the client's held set fixed, successive claims take the jobs in
    /// affinity order: both artifacts, the model, the runtime, then neither.
    #[tokio::test]
    async fn the_claim_ranks_both_artifacts_over_the_model_over_the_runtime() -> anyhow::Result<()>
    {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let at = now()?;
        let client = ClientId::try_new("picker")?;
        let lease_expiry = to_second(at) + chrono::Duration::hours(1);
        for (name, model, runtime) in [
            ("neither-job", "C", "R3"),
            ("runtime-job", "Z", "R1"),
            ("model-job", "A", "R9"),
            ("both-job", "A", "R1"),
        ] {
            let job = JobId::try_new(name)?;
            seed_artifact_job(&store, &client, &job, model, runtime, ExpiresAt::Never).await?;
        }
        let cached = CachedArtifacts {
            models: vec![descriptor("A")?],
            runtimes: vec![descriptor("R1")?],
        };

        let mut taken = Vec::new();
        for _ in 0..4 {
            let outcome = store
                .claim_for_client(&client, lease_expiry, 0, &[], None, Some(&cached), at)
                .await?;
            let ClaimOutcome::Granted {
                lease, affinity, ..
            } = outcome
            else {
                anyhow::bail!("expected a grant, got {outcome:?}");
            };
            store
                .delete_lease(&lease.job_id, lease.expires_at, &client, lease.lease_expiry)
                .await?;
            taken.push((lease.job_id.to_string(), affinity));
        }
        assert_eq!(
            taken,
            vec![
                ("both-job".to_string(), ClaimAffinity::Both),
                ("model-job".to_string(), ClaimAffinity::Model),
                ("runtime-job".to_string(), ClaimAffinity::Runtime),
                ("neither-job".to_string(), ClaimAffinity::None),
            ]
        );
        Ok(())
    }

    async fn granted_model(
        store: &SqlTodoStore,
        client: &ClientId,
        cached_model: &str,
    ) -> anyhow::Result<Option<String>> {
        let at = now()?;
        let cached = CachedArtifacts {
            models: vec![descriptor(cached_model)?],
            runtimes: Vec::new(),
        };
        let outcome = store
            .claim_for_client(
                client,
                at + chrono::Duration::hours(1),
                0,
                &[],
                None,
                Some(&cached),
                at,
            )
            .await?;
        let ClaimOutcome::Granted { body, .. } = outcome else {
            anyhow::bail!("expected a grant, got {outcome:?}");
        };
        Ok(body
            .pointer("/spec/model/name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned))
    }

    #[derive(Clone, Copy, Debug)]
    enum BJob {
        Plain,
        /// Its deadline is later than the job of model A.
        Later,
        /// A smaller run of its family is queued, so it ranks after the job of A.
        Waiting,
        /// It names no model, and neither do the leased jobs of B.
        NoModel,
    }

    /// A job another device leases, as `picker` sees it.
    #[derive(Clone, Copy, Debug)]
    enum Leased {
        Eligible,
        Ineligible,
        /// Eligible, but the lease lapsed.
        Lapsed,
        /// Eligible, on a runtime no candidate uses.
        OtherRuntime,
    }

    /// `picker` holds model A and may claim one job of model A and one of B.
    /// Each `leased` entry is a job another device leases: its model, and how
    /// `picker` sees it.
    #[rstest]
    #[case::a_model_no_other_device_runs_wins(&[("A", Leased::Eligible)], BJob::Plain, Some("B"))]
    #[case::equal_counts_go_to_the_held_model(&[("A", Leased::Eligible), ("B", Leased::Eligible)], BJob::Plain, Some("A"))]
    #[case::a_job_with_no_model_counts_zero(&[("A", Leased::Eligible), ("B", Leased::Eligible)], BJob::NoModel, None)]
    #[case::a_lease_the_device_cannot_run_does_not_count(&[("A", Leased::Ineligible)], BJob::Plain, Some("A"))]
    #[case::a_lapsed_lease_does_not_count(&[("A", Leased::Lapsed)], BJob::Plain, Some("A"))]
    #[case::a_lease_on_another_runtime_counts(&[("A", Leased::OtherRuntime)], BJob::Plain, Some("B"))]
    #[case::the_deadline_comes_first(&[("A", Leased::Eligible)], BJob::Later, Some("A"))]
    #[case::smaller_runs_rank_before_the_model_count(&[("A", Leased::Eligible)], BJob::Waiting, Some("A"))]
    #[tokio::test]
    async fn the_claim_prefers_the_model_the_fewest_other_devices_run(
        #[case] leased: &[(&str, Leased)],
        #[case] b_job: BJob,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let picker = ClientId::try_new("picker")?;
        let at = now()?;
        let body = |job: &JobId, model: &str, runtime: &str| match (b_job, model) {
            (BJob::NoModel, "B") => serde_json::json!({}),
            _ => artifact_body(job, model, runtime),
        };
        stream::iter(leased.iter().enumerate())
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|(index, &(model, lease))| {
                let (store, picker, body) = (&store, &picker, &body);
                async move {
                    let job = JobId::try_new(format!("leased-{index}"))?;
                    let holder = ClientId::try_new(format!("holder-{index}"))?;
                    let runtime = if matches!(lease, Leased::OtherRuntime) {
                        "R2"
                    } else {
                        "R"
                    };
                    store.write_tmp(&job, &body(&job, model, runtime)).await?;
                    store.promote_avail(&job, ExpiresAt::Never, None).await?;
                    if !matches!(lease, Leased::Ineligible) {
                        store.write_eligible(picker, &job, ExpiresAt::Never).await?;
                    }
                    let lease_expiry = if matches!(lease, Leased::Lapsed) {
                        at - chrono::Duration::minutes(1)
                    } else {
                        at + chrono::Duration::hours(1)
                    };
                    store
                        .claim_job(&job, ExpiresAt::Never, &holder, lease_expiry)
                        .await?;
                    Ok(())
                }
            })
            .await?;

        let soon = ExpiresAt::At(to_second(at) + chrono::Duration::days(1));
        seed_artifact_job(&store, &picker, &JobId::try_new("a-job")?, "A", "R", soon).await?;
        let b_deadline = if matches!(b_job, BJob::Later) {
            ExpiresAt::Never
        } else {
            soon
        };
        let longer: &[i64] = &[512];
        let family = matches!(b_job, BJob::Waiting).then_some(("family", longer));
        let b = JobId::try_new("b-job")?;
        store.write_tmp(&b, &body(&b, "B", "R")).await?;
        store.promote_avail(&b, b_deadline, family).await?;
        store.write_eligible(&picker, &b, b_deadline).await?;
        if family.is_some() {
            let shorter = JobId::try_new("b-shorter")?;
            store.write_tmp(&shorter, &body(&shorter, "B", "R")).await?;
            store
                .promote_avail(&shorter, b_deadline, Some(("family", [256].as_slice())))
                .await?;
        }

        assert_eq!(
            granted_model(&store, &picker, "A").await?.as_deref(),
            expected
        );
        Ok(())
    }

    /// Four devices that all hold model A claim in turn among three jobs of A
    /// and three of B. Each takes the model fewer of the others run, A on a
    /// tie, so two devices run each model.
    #[tokio::test]
    async fn devices_that_hold_one_model_split_across_the_models() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let devices = (0..4)
            .map(|n| ClientId::try_new(format!("phone-{n}")))
            .collect::<Result<Vec<_>, _>>()?;
        let jobs = [
            ("A", "a-0"),
            ("A", "a-1"),
            ("A", "a-2"),
            ("B", "b-0"),
            ("B", "b-1"),
            ("B", "b-2"),
        ];
        stream::iter(jobs)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|(model, name)| {
                let (store, devices) = (&store, &devices);
                async move {
                    let job = JobId::try_new(name)?;
                    store
                        .write_tmp(&job, &artifact_body(&job, model, "R"))
                        .await?;
                    store.promote_avail(&job, ExpiresAt::Never, None).await?;
                    stream::iter(devices)
                        .map(Ok::<_, anyhow::Error>)
                        .try_for_each(|device| store.write_eligible(device, &job, ExpiresAt::Never))
                        .await
                }
            })
            .await?;

        let taken: Vec<Option<String>> = stream::iter(&devices)
            .then(|device| granted_model(&store, device, "A"))
            .try_collect()
            .await?;
        assert_eq!(
            taken,
            ["A", "B", "A", "B"].map(|model| Some(model.to_owned()))
        );
        Ok(())
    }

    /// Whitespace and key order in a body do not change its keys.
    #[tokio::test]
    async fn the_artifact_keys_ignore_formatting() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let pool = test_db.db().pool();
        for (job, body) in [
            (
                "compact",
                r#"{"spec":{"model":{"a":1,"bb":"x"},"runtime":{"c":[1,2]}}}"#,
            ),
            (
                "spaced",
                r#"{ "spec" : { "runtime" : { "c" : [ 1 , 2 ] }, "model" : { "bb" : "x", "a" : 1 } } }"#,
            ),
        ] {
            sqlx::query("INSERT INTO queue_jobs (job_id, body, revision) VALUES ($1, $2, 1)")
                .bind(job)
                .bind(body)
                .execute(pool)
                .await?;
        }
        let keys: Vec<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT model_key, runtime_key FROM queue_jobs ORDER BY job_id")
                .fetch_all(pool)
                .await?;
        assert_eq!(keys.len(), 2);
        assert!(keys[0].0.is_some() && keys[0].1.is_some());
        assert_eq!(keys[0], keys[1]);
        Ok(())
    }

    /// The eligibility, denial and lost-lease rows that name `job`.
    async fn rows_naming(test_db: &crate::scratch::TestDb, job: &JobId) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM queue_eligibility WHERE job_id = $1) \
             + (SELECT count(*) FROM queue_denials WHERE job_id = $1) \
             + (SELECT count(*) FROM queue_lost_leases WHERE job_id = $1)",
        )
        .bind(job.as_str())
        .fetch_one(test_db.db().pool())
        .await?)
    }

    async fn write_each_row(
        store: &SqlTodoStore,
        job: &JobId,
        client: &ClientId,
    ) -> anyhow::Result<()> {
        store.write_eligible(client, job, ExpiresAt::Never).await?;
        store
            .write_eligible_rows(client, std::slice::from_ref(job))
            .await?;
        store
            .write_denied(job, client, &DenialRecord::default())
            .await?;
        store.write_lost_lease(job, client, now()?).await
    }

    #[tokio::test]
    async fn writes_for_a_missing_job_store_nothing() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("never-queued")?;
        write_each_row(&store, &job, &ClientId::try_new("ev1_writer")?).await?;
        assert_eq!(rows_naming(&test_db, &job).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_lost_lease_for_a_deleted_job_forgets_its_last_grant() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("deleted-grant")?;
        let client = ClientId::try_new("ev1_holder")?;
        register_client(&test_db, client.clone()).await?;
        let at = now()?;
        enqueue(&store, &job).await?;
        store
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        let outcome = store
            .claim_for_client(
                &client,
                at + chrono::Duration::hours(1),
                0,
                &[],
                None,
                None,
                at,
            )
            .await?;
        let ClaimOutcome::Granted { lease, .. } = outcome else {
            anyhow::bail!("expected a grant, got {outcome:?}");
        };
        store
            .delete_lease(&job, lease.expires_at, &client, lease.lease_expiry)
            .await?;
        let last_grants = || {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM queue_client_last_grant WHERE client_id = $1",
            )
            .bind(client.as_str())
            .fetch_one(test_db.db().pool())
        };
        assert_eq!(last_grants().await?, 1);

        store
            .write_lost_lease(&job, &client, lease.lease_expiry)
            .await?;
        assert_eq!(last_grants().await?, 0);
        assert_eq!(rows_naming(&test_db, &job).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_client_forgets_its_completed_grant() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let clients = crate::clients::SqlClientStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_completed")?;
        let job = JobId::try_new("completed-grant")?;
        let at = now()?;
        register_client(&test_db, client.clone()).await?;
        seed_artifact_job(&store, &client, &job, "model", "runtime", ExpiresAt::Never).await?;
        let outcome = store
            .claim_for_client(
                &client,
                at + chrono::Duration::hours(1),
                0,
                &[],
                None,
                None,
                at,
            )
            .await?;
        let ClaimOutcome::Granted { lease, .. } = outcome else {
            anyhow::bail!("expected a grant, got {outcome:?}");
        };
        store
            .delete_lease(&job, lease.expires_at, &client, lease.lease_expiry)
            .await?;
        let count = || {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM queue_client_last_grant")
                .fetch_one(test_db.db().pool())
        };
        assert_eq!(count().await?, 1);
        clients.delete_client(&client).await?;
        assert_eq!(count().await?, 0);

        register_client(&test_db, client.clone()).await?;
        let mut tx = test_db.db().begin_read_committed().await?;
        assert_eq!(
            cached_keys(&mut tx, client.as_str(), None).await?,
            (Vec::<String>::new(), Vec::<String>::new())
        );
        tx.commit().await?;
        Ok(())
    }

    async fn grant_for_deletion_race(
        store: &SqlTodoStore,
        client: &ClientId,
        job: &JobId,
        reclaim: bool,
    ) -> anyhow::Result<()> {
        let at = now()?;
        let until = at + chrono::Duration::hours(1);
        if reclaim {
            let outcome = store.reclaim_for_client(job, client, until, 0, at).await?;
            anyhow::ensure!(
                matches!(
                    outcome,
                    ReclaimOutcome::Granted { .. } | ReclaimOutcome::Renewed
                ),
                "expected a reclaim, got {outcome:?}"
            );
        } else {
            let outcome = store
                .claim_for_client(client, until, 0, &[], None, None, at)
                .await?;
            anyhow::ensure!(
                matches!(outcome, ClaimOutcome::Granted { .. }),
                "expected a grant, got {outcome:?}"
            );
        }
        Ok(())
    }

    #[rstest]
    #[case::claim(false)]
    #[case::reclaim(true)]
    #[tokio::test]
    async fn lapsed_grant_settlement_racing_client_deletion_does_not_deadlock(
        #[case] reclaim: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let client = ClientId::try_new("ev1_lapsed_delete")?;
        let old = JobId::try_new("old-grant")?;
        let next = JobId::try_new("next-grant")?;
        let at = now()?;
        register_client(&test_db, client.clone()).await?;
        seed_artifact_job(&store, &client, &old, "old", "runtime", ExpiresAt::Never).await?;
        let outcome = store
            .claim_for_client(
                &client,
                at - chrono::Duration::minutes(1),
                0,
                &[],
                None,
                None,
                at - chrono::Duration::minutes(2),
            )
            .await?;
        anyhow::ensure!(
            matches!(outcome, ClaimOutcome::Granted { .. }),
            "expected an old grant"
        );
        seed_artifact_job(&store, &client, &next, "next", "runtime", ExpiresAt::Never).await?;

        let mut deletion = test_db.db().begin_read_committed().await?;
        sqlx::query("SELECT client_id FROM clients WHERE client_id = $1 FOR UPDATE")
            .bind(client.as_str())
            .execute(&mut *deletion)
            .await?;
        let claim = tokio::spawn({
            let store = store.clone();
            let client = client.clone();
            async move { grant_for_deletion_race(&store, &client, &next, reclaim).await }
        });
        // Wait for the claim to reach the locked parent before attempting the
        // cascade. Taking the grant lock first would deadlock these transactions.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE \
                     application_name = current_setting('application_name') \
                     AND wait_event_type = 'Lock' AND query LIKE '%clients%')",
                )
                .fetch_one(test_db.db().pool())
                .await?;
                if waiting {
                    return anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("the claim must reach the locked client row")??;
        sqlx::query("DELETE FROM clients WHERE client_id = $1")
            .bind(client.as_str())
            .execute(&mut *deletion)
            .await?;
        deletion.commit().await?;
        claim.await??;
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM queue_client_last_grant")
            .fetch_one(test_db.db().pool())
            .await?;
        assert_eq!(remaining, 0);
        Ok(())
    }

    #[rstest]
    #[case::claim_new(false, false)]
    #[case::claim_existing(false, true)]
    #[case::reclaim_new(true, false)]
    #[case::reclaim_existing(true, true)]
    #[tokio::test]
    async fn a_grant_racing_client_deletion_leaves_no_affinity(
        #[case] reclaim: bool,
        #[case] existing: bool,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let clients = crate::clients::SqlClientStore::new(test_db.db().clone());
        let fixtures = (0..RACERS / 2)
            .map(|pair| {
                Ok((
                    ClientId::try_new(format!("grant_race_{pair}"))?,
                    JobId::try_new(format!("job-{pair}"))?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        stream::iter(&fixtures)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|(client, job)| {
                let test_db = &test_db;
                let store = &store;
                async move {
                    register_client(test_db, client.clone()).await?;
                    enqueue(store, job).await?;
                    store.write_eligible(client, job, ExpiresAt::Never).await?;
                    if existing {
                        grant_for_deletion_race(store, client, job, false).await?;
                        store
                            .recycle_lease(
                                job,
                                ExpiresAt::Never,
                                client,
                                now()? + chrono::Duration::hours(1),
                            )
                            .await?;
                    }
                    anyhow::Ok(())
                }
            })
            .await?;
        race(|n| {
            let (client, job) = fixtures[n / 2].clone();
            let store = store.clone();
            let clients = clients.clone();
            async move {
                if n % 2 == 0 {
                    clients.delete_client(&client).await
                } else {
                    grant_for_deletion_race(&store, &client, &job, reclaim).await
                }
            }
        })
        .await?;

        let (client, job) = &fixtures[0];
        store
            .recycle_lease(
                job,
                ExpiresAt::Never,
                client,
                now()? + chrono::Duration::hours(1),
            )
            .await?;
        grant_for_deletion_race(&store, client, job, reclaim).await?;
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM queue_client_last_grant")
            .fetch_one(test_db.db().pool())
            .await?;
        assert_eq!(remaining, 0);
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum TerminalDelete {
        Avail,
        Lease,
    }

    #[rstest]
    #[case(TerminalDelete::Avail)]
    #[case(TerminalDelete::Lease)]
    #[tokio::test]
    async fn a_terminal_delete_takes_the_jobs_rows(
        #[case] how: TerminalDelete,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("finished")?;
        let client = ClientId::try_new("ev1_finisher")?;
        enqueue(&store, &job).await?;
        write_each_row(&store, &job, &client).await?;
        assert_eq!(rows_naming(&test_db, &job).await?, 3);
        match how {
            TerminalDelete::Avail => store.delete_avail_by_job(&job).await?,
            TerminalDelete::Lease => {
                let at = now()?;
                store.claim_job(&job, ExpiresAt::Never, &client, at).await?;
                store
                    .delete_lease(&job, ExpiresAt::Never, &client, at)
                    .await?;
            }
        }
        assert!(store.get_avail_by_job(&job).await?.is_none());
        assert_eq!(rows_naming(&test_db, &job).await?, 0);
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
        assert!(
            store
                .avail_for_jobs(std::slice::from_ref(&job))
                .await?
                .is_empty()
        );
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
            store.avail_for_jobs(std::slice::from_ref(&job)).await?,
            vec![AvailEntry {
                job_id: job.clone(),
                expires_at: ExpiresAt::At(at.trunc_subsecs(0))
            }]
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

    /// Queue `job` at `deadline`, with one membership marker per `(plan, horizon)`.
    async fn enqueue_held(
        test_db: &TestDb,
        store: &SqlTodoStore,
        job: &JobId,
        deadline: ExpiresAt,
        markers: &[(&str, &str)],
    ) -> anyhow::Result<()> {
        store
            .write_tmp(job, &serde_json::json!({"job": job}))
            .await?;
        store.promote_avail(job, deadline, None).await?;
        stream::iter(markers)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|(plan, horizon)| async move {
                sqlx::query(
                    "INSERT INTO job_plans (job_id, plan_id, expires_at, written_at) VALUES \
                     ($1, $2, $3, now())",
                )
                .bind(job.as_str())
                .bind(plan)
                .bind(horizon)
                .execute(test_db.db().pool())
                .await?;
                anyhow::Ok(())
            })
            .await
    }

    /// A waiting job takes the earliest horizon of its member plans that has
    /// not passed, earlier or later than its row. A plan's horizon is its
    /// latest marker, and `never` ranks after every date.
    #[rstest]
    #[case::two_plans_take_the_earlier(
        "20270210T000000Z",
        &[("a", "20261210T000000Z"), ("b", "20270210T000000Z")],
        "20261210T000000Z"
    )]
    #[case::a_plan_counts_its_latest_marker(
        "20270210T000000Z",
        &[("a", "20261201T000000Z"), ("a", "20270301T000000Z"), ("b", "20270201T000000Z")],
        "20270201T000000Z"
    )]
    #[case::a_date_ranks_before_never(
        "never",
        &[("a", "never"), ("b", "20261210T000000Z")],
        "20261210T000000Z"
    )]
    #[case::all_never_is_never("20270101T000000Z", &[("a", "never"), ("b", "never")], "never")]
    #[case::a_passed_horizon_is_left_out(
        "20270210T000000Z",
        &[("a", "20261101T083000Z"), ("b", "20261215T000000Z"), ("c", "20270101T000000Z")],
        "20261215T000000Z"
    )]
    #[case::a_malformed_marker_is_left_out(
        "20270210T000000Z",
        &[
            ("a", "20261210T000000Z"),
            ("b", "20261340T000000Z"),
            ("c", "2026-11-20"),
            ("not.a.plan", "20261120T000000Z"),
        ],
        "20261210T000000Z"
    )]
    #[case::a_later_horizon_moves_the_row_later(
        "20261201T000000Z",
        &[("a", "20270101T000000Z")],
        "20270101T000000Z"
    )]
    #[case::no_live_member_keeps_the_row(
        "20270210T000000Z",
        &[("a", "20261031T000000Z")],
        "20270210T000000Z"
    )]
    #[case::no_member_keeps_the_row("20270210T000000Z", &[], "20270210T000000Z")]
    #[case::a_matching_row_is_left_alone(
        "20261210T000000Z",
        &[("a", "20261210T000000Z")],
        "20261210T000000Z"
    )]
    #[tokio::test]
    async fn a_waiting_job_takes_its_earliest_live_member_horizon(
        #[case] row: &str,
        #[case] markers: &[(&str, &str)],
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("held-job")?;
        let row: ExpiresAt = row.parse()?;
        let expected: ExpiresAt = expected.parse()?;
        enqueue_held(&test_db, &store, &job, row, markers).await?;

        assert_eq!(
            store.restamp_avail_deadlines(now()?).await?,
            u64::from(expected != row)
        );
        assert_eq!(
            store.avail_for_jobs(std::slice::from_ref(&job)).await?,
            vec![AvailEntry {
                job_id: job.clone(),
                expires_at: expected
            }]
        );
        assert_eq!(store.restamp_avail_deadlines(now()?).await?, 0);
        Ok(())
    }

    /// A lease is named by its job's deadline, so a leased job keeps it until
    /// it is back in the queue.
    #[tokio::test]
    async fn a_leased_job_is_restamped_once_back_in_the_queue() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlTodoStore::new(test_db.db().clone());
        let job = JobId::try_new("leased-held-job")?;
        let client = ClientId::try_new("restamp_holder")?;
        let row: ExpiresAt = "20270210T000000Z".parse()?;
        let member: ExpiresAt = "20261210T000000Z".parse()?;
        enqueue_held(&test_db, &store, &job, row, &[("a", "20261210T000000Z")]).await?;
        let lease_expiry = now()? + chrono::Duration::hours(1);
        assert!(matches!(
            store.claim_job(&job, row, &client, lease_expiry).await?,
            ClaimResult::Claimed(_)
        ));

        assert_eq!(store.restamp_avail_deadlines(now()?).await?, 0);
        assert!(
            store
                .get_leased(&job, row, &client, lease_expiry)
                .await?
                .is_some()
        );
        assert_eq!(
            store
                .recycle_lease(&job, row, &client, lease_expiry)
                .await?,
            RecycleResult::Recycled
        );
        assert_eq!(store.restamp_avail_deadlines(now()?).await?, 1);
        assert_eq!(
            store.avail_for_jobs(std::slice::from_ref(&job)).await?,
            vec![AvailEntry {
                job_id: job.clone(),
                expires_at: member
            }]
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
        store.promote_avail(&job, ExpiresAt::Never, None).await?;
        assert!(
            store
                .promote_avail(&job, ExpiresAt::At(now()?), None)
                .await
                .is_err()
        );
        store
            .write_tmp(&job, &serde_json::json!({"replacement": true}))
            .await?;
        store
            .promote_avail(&job, ExpiresAt::At(now()?), None)
            .await?;
        assert_eq!(store.get_avail_by_job(&job).await?, body);
        assert_eq!(
            store.avail_for_jobs(std::slice::from_ref(&job)).await?,
            vec![AvailEntry {
                job_id: job.clone(),
                expires_at: ExpiresAt::Never
            }]
        );
        assert_eq!(store.delete_stale_tmp(Duration::ZERO).await?, 0);
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
