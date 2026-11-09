//! PostgreSQL storage for queue jobs, leases, markers, and maintenance state.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};

use pipette_core::denial::DenialRecord;
use pipette_core::todo_filename::{Lease, avail_filename, leased_key, tmp_filename};
use pipette_plan_types::{ClientId, ExpiresAt, FailureCode, JobId};
use pipette_store::{
    AvailEntry, BacklogSnapshot, CampaignBacklogSnapshot, ClaimOutcome, ClaimResult,
    LivenessImport, ReclaimOutcome, RecycleResult, RedateResult, ReindexBatch, ReindexJob,
    RenewLeaseResult, TodoStore,
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

/// Serializes one client's claims until the transaction ends, so the client cannot
/// acquire two leases at once. Two clients whose IDs hash alike only
/// wait for each other.
async fn lock_client(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    client: &str,
) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(client)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Recycle lapsed leases under the client lock. Terminal records and pending
/// repairs need object-store work; keep those jobs leased until it completes.
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
        sqlx::query(
            "INSERT INTO queue_lost_leases (job_id, client_id, lease_expiry) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
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

    fn key(self) -> Option<String> {
        let lease = self.lease()?;
        Some(leased_key(
            &lease.job_id,
            lease.expires_at,
            &lease.client_id,
            lease.lease_expiry,
        ))
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
                sqlx::query(
                    "INSERT INTO queue_lost_leases (job_id, client_id, lease_expiry) VALUES ($1, \
                     $2, $3) ON CONFLICT DO NOTHING",
                )
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
            // `is_expired` keeps a job whose deadline is exactly `now`.
            let pick: Option<(String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
                "SELECT j.job_id, j.body, j.expires_at FROM queue_eligibility e JOIN queue_jobs j \
                 ON j.job_id = e.job_id WHERE e.client_id = $1 AND j.leased_by IS NULL AND \
                 (j.expires_at IS NULL OR j.expires_at >= $2) AND NOT EXISTS (SELECT 1 FROM \
                 queue_denials d WHERE d.job_id = j.job_id AND d.client_id = $1) AND NOT \
                 (j.job_id = ANY($3)) ORDER BY j.expires_at ASC NULLS LAST, random() LIMIT 1 \
                 FOR UPDATE OF j SKIP LOCKED",
            )
            .bind(client)
            .bind(now)
            .bind(&passed_over)
            .fetch_optional(&mut *tx)
            .await?;
            self.record_body_reads(usize::from(pick.is_some()));
            match pick {
                Some((job, body, expires_at)) => {
                    sqlx::query(
                        "UPDATE queue_jobs SET leased_by = $2, lease_expiry = $3, revision = \
                         revision + 1 WHERE job_id = $1",
                    )
                    .bind(&job)
                    .bind(client)
                    .bind(lease_expiry)
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
                    ClaimOutcome::Granted { lease, body }
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
        tx.commit().await?;
        Ok(outcome)
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
                     failure_code, client_version, runtime_version) VALUES ($1, $2, $3, $4, $5, \
                     $6, $7) ON CONFLICT (job_id, client_id) DO UPDATE SET denied_at = \
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
                            code.parse::<FailureCode>().map_err(|e| {
                                anyhow::anyhow!("queue_denials.failure_code {code:?}: {e}")
                            })
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
                    "INSERT INTO queue_eligibility (client_id, job_id) VALUES ($1, $2) ON \
                     CONFLICT DO NOTHING",
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
                    "INSERT INTO queue_eligibility (client_id, job_id) SELECT $1, unnest($2::text[]) \
                     ON CONFLICT DO NOTHING",
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
    use futures::stream::{self, StreamExt, TryStreamExt};
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
        store.promote_avail(&job, ExpiresAt::Never).await?;
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
        Ok(())
    }

    #[tokio::test]
    async fn projection_preserves_nested_and_escaped_matcher_values() -> anyhow::Result<()> {
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
        stream::iter([
            serde_json::to_string(&body)?,
            serde_json::to_string_pretty(&body)?,
        ])
        .map(Ok::<_, anyhow::Error>)
        .try_for_each(|encoded| async {
            let actual: serde_json::Value = sqlx::query_scalar("SELECT queue_match_clauses($1)")
                .bind(encoded)
                .fetch_one(test_db.db().pool())
                .await?;
            assert_eq!(actual, expected);
            anyhow::Ok(())
        })
        .await?;
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
        store.promote_avail(&job, ExpiresAt::Never).await?;
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

    #[rstest]
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
        assert!(store.read_gc_candidates().await?.is_empty());
        assert!(store.read_backlog().await?.is_none());
        assert!(store.read_campaign_backlog().await?.is_none());
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
        store.promote_avail(&job, old).await?;
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
        for job in store.jobs_needing_reindex(jobs).await?.jobs {
            store
                .replace_eligibility(&job.job_id, job.revision, &[])
                .await?;
        }
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
        store.promote_avail(&job, dated).await?;
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
                    .claim_for_client(&client, lease_expiry, 0, &[], None, at)
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
                    .claim_for_client(&client, until, 3, &[], None, at)
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
                .claim_for_client(&client, at + chrono::Duration::minutes(5), 3, &[], None, at)
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
            .claim_for_client(&client, until, 2, &[], None, at)
            .await?;
        let ClaimOutcome::CheckHeldLease { lease, .. } = result else {
            anyhow::bail!("a held lease needs an access check before loss accounting");
        };
        assert!(store.list_lost_leases(&job).await?.is_empty());
        let result = store
            .claim_for_client(&client, until, 2, &[], Some(&lease), at)
            .await?;
        assert!(matches!(result, ClaimOutcome::HandedBack { .. }));
        assert_eq!(store.list_lost_leases(&job).await?.len(), 1);

        // A heartbeat changes the lease while its access check is in flight.
        let renewed = until + chrono::Duration::minutes(1);
        store.renew_lease(&job, &client, renewed).await?;
        let result = store
            .claim_for_client(&client, renewed, 2, &[], Some(&lease), at)
            .await?;
        let ClaimOutcome::CheckHeldLease { lease, .. } = result else {
            anyhow::bail!("the renewed lease must pass its own access check");
        };
        assert_eq!(store.list_lost_leases(&job).await?.len(), 1);
        let result = store
            .claim_for_client(&client, renewed, 2, &[], Some(&lease), at)
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
            store.promote_avail(job, deadline).await?;
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
                at,
            )
            .await?;
        let ClaimOutcome::Granted { lease, .. } = outcome else {
            anyhow::bail!("expected a grant, got {outcome:?}");
        };
        assert_eq!(lease.job_id.as_str(), expected);
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
