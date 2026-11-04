//! [`SubmissionStore`] with an index of record outcomes on SQL and the records
//! on the wrapped object store. A read answers a job from its row. A job with a
//! pending repair reads its record from the wrapped store, and a job with no row
//! and no pending repair has no record. `docs/storage.md` gives the row rules
//! and the remaining windows.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};

use pipette_core::plan::SettledOutcome;
use pipette_plan_types::{ClientId, JobId, RuntimeOutput};
use pipette_store::{
    CreateOutcome, OutcomeRepair, PruneSummary, RecordedRun, ScoreQueueStage, SubmissionRecord,
    SubmissionStore, UnreadableObjectError,
};

use crate::db::SqlDatabase;
use crate::retry::retry;

const REPAIR_MIN_AGE: Duration = Duration::from_secs(15 * 60);
const REPAIR_PAGE: i64 = 500;
const REPAIR_READS: usize = 4;
/// The job ids that a [`MissingRunFlagsError`] names.
const MISSING_FLAGS_NAMED: usize = 10;

/// The `outcome` and `run_flags` columns of a row in `submission_outcomes`.
type OutcomeRow = (SettledOutcome, Option<Vec<String>>);

/// The job id, `outcome`, `run_flags`, and pending repair of one job in a
/// summary read.
type SummaryRow = (String, Option<String>, Option<Vec<String>>, bool);

struct Summaries {
    /// Each job with a row and no pending repair, with its row.
    rows: Vec<(JobId, OutcomeRow)>,
    /// Each job with a pending repair. Its record gives the answer.
    pending: Vec<JobId>,
}

/// A run read finds `finished` rows with no `run_flags`. The flags of such a
/// run are unknown, so the read gives no answer for it.
#[derive(Debug, thiserror::Error)]
#[error(
    "{count} finished submission outcome rows have no run flags (first: {}); \
     run `pipette-mgmt db backfill-job-cells` to fill them",
    .first.join(", ")
)]
pub(crate) struct MissingRunFlagsError {
    count: usize,
    first: Vec<String>,
}

/// The caller of [`SqlSubmissionStore::publish`]. It decides whether the job
/// must still be pending and what a generation mismatch does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Publisher {
    /// A body writer. It publishes while the job is pending or not, and a
    /// mismatch reopens the repair.
    Writer,
    /// The repair pass, or a create that puts back the row of an older body.
    /// It publishes only while the job is pending, and a mismatch leaves the
    /// repair as it is.
    Repair,
}

#[derive(Clone)]
pub(crate) struct SqlSubmissionStore {
    db: SqlDatabase,
    records: Arc<dyn SubmissionStore>,
}

impl SqlSubmissionStore {
    pub(crate) fn new(db: SqlDatabase, records: Arc<dyn SubmissionStore>) -> Self {
        Self { db, records }
    }

    /// Commit the marker and row deletion before any object operation. Give
    /// the generation and the deleted row. A row whose outcome does not parse
    /// is dropped, and the repair pass rebuilds it.
    async fn invalidate(&self, job_id: &JobId) -> anyhow::Result<(i64, Option<OutcomeRow>)> {
        retry("invalidate_outcome", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let (generation,): (i64,) = sqlx::query_as(
                    "INSERT INTO submission_outcome_repairs (job_id, generation, pending)
                 VALUES ($1, 1, true) ON CONFLICT (job_id) DO UPDATE
                 SET generation = submission_outcome_repairs.generation + 1,
                     pending = true, updated_at = now()
                 RETURNING generation",
                )
                .bind(job_id.as_str())
                .fetch_one(&mut *tx)
                .await?;
                let old: Option<(String, Option<Vec<String>>)> = sqlx::query_as(
                "DELETE FROM submission_outcomes WHERE job_id = $1 RETURNING outcome, run_flags")
                .bind(job_id.as_str()).fetch_optional(&mut *tx).await?;
                tx.commit().await?;
                let old = old.and_then(|(outcome, flags)| Some((outcome.parse().ok()?, flags)));
                Ok((generation, old))
            })
        })
        .await
        .inspect_err(|e| {
            tracing::warn!(job_id = %job_id, error = %e,
            "outcome repair could not be marked pending before a body write")
        })
    }

    async fn publish(
        &self,
        job_id: &JobId,
        generation: i64,
        row: Option<OutcomeRow>,
        publisher: Publisher,
    ) -> anyhow::Result<bool> {
        retry("publish_outcome", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let matched: Option<(i64,)> = sqlx::query_as(
                    "UPDATE submission_outcome_repairs SET pending = false, updated_at = now()
                 WHERE job_id = $1 AND generation = $2 AND ($3 = false OR pending)
                 RETURNING generation",
                )
                .bind(job_id.as_str())
                .bind(generation)
                .bind(publisher == Publisher::Repair)
                .fetch_optional(&mut *tx)
                .await?;
                if matched.is_some() {
                    if let Some((outcome, flags)) = &row {
                        sqlx::query(
                            "INSERT INTO submission_outcomes (job_id, outcome, run_flags)
                        VALUES ($1, $2, $3) ON CONFLICT (job_id) DO UPDATE
                        SET outcome = EXCLUDED.outcome, run_flags = EXCLUDED.run_flags",
                        )
                        .bind(job_id.as_str())
                        .bind(outcome.label())
                        .bind(flags.as_deref())
                        .execute(&mut *tx)
                        .await?;
                    }
                } else if publisher == Publisher::Writer {
                    sqlx::query(
                        "INSERT INTO submission_outcome_repairs (job_id, generation, pending)
                    VALUES ($1, 1, true) ON CONFLICT (job_id) DO UPDATE
                    SET generation = submission_outcome_repairs.generation + 1,
                        pending = true, updated_at = now()",
                    )
                    .bind(job_id.as_str())
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("DELETE FROM submission_outcomes WHERE job_id = $1")
                        .bind(job_id.as_str())
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(matched.is_some())
            })
        })
        .await
    }

    /// Put back the row that `invalidate` deleted, after a create that wrote no
    /// body. A failure leaves the job pending for the repair pass.
    async fn put_back(&self, job_id: &JobId, generation: i64, old: Option<OutcomeRow>) {
        let Some(old) = old else { return };
        if let Err(e) = self
            .publish(job_id, generation, Some(old), Publisher::Repair)
            .await
        {
            tracing::warn!(job_id = %job_id, generation, error = %e,
                "outcome row could not be put back after a create that wrote no body");
        }
    }

    /// Mark the outcome of `job_id` pending and delete its row, run `write` to
    /// replace its body with `body`, then publish the row of `body` if the
    /// generation is unchanged.
    async fn replace(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
        write: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> anyhow::Result<()> {
        let (generation, _) = self.invalidate(job_id).await?;
        let store = self.clone();
        let job_id = job_id.clone();
        let row = outcome_row(body);
        // Keep the body write and its SQL publication together if the caller
        // is cancelled. A late object-store write must still publish its row.
        tokio::spawn(async move {
            write.await.inspect_err(|e| {
                tracing::warn!(job_id = %job_id, generation, error = %e,
                    "body write stopped before publishing its outcome")
            })?;
            let published = store
                .publish(&job_id, generation, Some(row), Publisher::Writer)
                .await
                .inspect_err(|e| {
                    tracing::warn!(job_id = %job_id, generation, error = %e,
                        "outcome row could not be published after a body replacement")
                })?;
            if !published {
                tracing::warn!(job_id = %job_id, generation,
                    "outcome repair reopened: generation moved during the body write");
            }
            Ok(())
        })
        .await
        .context("body write task ended before publishing its outcome")?
    }

    /// Move a pending repair to the back of the page order for a later pass.
    /// A failure leaves the repair where it is.
    async fn defer(&self, job_id: &str, generation: i64) {
        let deferred = self
            .db
            .execute("defer_outcome_repair", || {
                sqlx::query(
                    "UPDATE submission_outcome_repairs SET updated_at = now()
                 WHERE job_id = $1 AND generation = $2 AND pending",
                )
                .bind(job_id.to_owned())
                .bind(generation)
            })
            .await;
        if let Err(e) = deferred {
            tracing::warn!(job_id, generation, error = %e,
                "outcome repair could not be deferred");
        }
    }

    /// Read runs and the pending repairs of `job_ids` in one statement. A job
    /// that has neither is absent from both lists.
    /// Each job is in the result once, however often `job_ids` names it.
    async fn summaries(&self, job_ids: &[JobId]) -> anyhow::Result<Summaries> {
        let asked: HashMap<&str, &JobId> = job_ids.iter().map(|id| (id.as_str(), id)).collect();
        let ids: Vec<&str> = asked.keys().copied().collect();
        let found: Vec<SummaryRow> = self
            .db
            .fetch_all("find_runs", || {
                sqlx::query_as(
                    "SELECT asked.job_id, o.outcome, o.run_flags, r.job_id IS NOT NULL
                     FROM unnest($1::text[]) AS asked (job_id)
                     LEFT JOIN submission_outcomes o ON o.job_id = asked.job_id
                     LEFT JOIN submission_outcome_repairs r
                         ON r.job_id = asked.job_id AND r.pending
                     WHERE o.job_id IS NOT NULL OR r.job_id IS NOT NULL",
                )
                .bind(ids.as_slice())
            })
            .await?;
        found
            .into_iter()
            .filter_map(|(job_id, outcome, run_flags, pending)| {
                Some((
                    (*asked.get(job_id.as_str())?).clone(),
                    outcome,
                    run_flags,
                    pending,
                ))
            })
            .try_fold(
                Summaries {
                    rows: Vec::new(),
                    pending: Vec::new(),
                },
                |mut summaries, (job_id, outcome, run_flags, pending)| {
                    match (pending, outcome) {
                        (true, _) => summaries.pending.push(job_id),
                        (false, Some(outcome)) => {
                            let outcome = outcome
                                .parse()
                                .with_context(|| format!("submission_outcomes row for {job_id}"))?;
                            summaries.rows.push((job_id, (outcome, run_flags)));
                        }
                        (false, None) => {}
                    }
                    anyhow::Ok(summaries)
                },
            )
    }

    async fn repair_outcomes_with(
        &self,
        min_age: Duration,
        limit: i64,
    ) -> anyhow::Result<OutcomeRepair> {
        let seconds = i64::try_from(min_age.as_secs())?;
        let candidates: Vec<(String, i64)> = self
            .db
            .fetch_all("outcome_repair_page", || {
                sqlx::query_as(
                    "SELECT job_id, generation FROM submission_outcome_repairs
                WHERE pending AND updated_at <= now() - ($1 * interval '1 second')
                ORDER BY updated_at, job_id LIMIT $2",
                )
                .bind(seconds)
                .bind(limit)
            })
            .await?;
        let mut report = OutcomeRepair::default();
        let reads = stream::iter(candidates)
            .map(|(id, generation)| async move {
                let read = match JobId::try_new(&id) {
                    Ok(job_id) => {
                        let record = self.records.find_job(&job_id).await;
                        Ok((job_id, record))
                    }
                    Err(e) => Err(e),
                };
                (id, generation, read)
            })
            .buffer_unordered(REPAIR_READS);
        futures::pin_mut!(reads);
        while let Some((id, generation, read)) = reads.next().await {
            report.attempted += 1;
            let (job_id, record) = match read {
                Ok(read) => read,
                Err(e) => {
                    report.failed += 1;
                    tracing::warn!(job_id = %id, generation, error = %e,
                        "outcome repair skipped: the job id is invalid");
                    self.defer(&id, generation).await;
                    continue;
                }
            };
            let record = match record {
                Ok(record) => record,
                Err(e) => {
                    report.failed += 1;
                    if is_unreadable_record(&e) {
                        tracing::warn!(job_id = %job_id, error = %e,
                            "outcome repair cannot read a damaged submission record");
                    } else {
                        tracing::warn!(job_id = %job_id, error = %e,
                            "outcome repair could not read the submission record");
                    }
                    self.defer(job_id.as_str(), generation).await;
                    continue;
                }
            };
            if record.is_none() {
                match self.records.has_record(&job_id).await {
                    Ok(false) => {}
                    Ok(true) => {
                        report.deferred += 1;
                        tracing::debug!(job_id = %job_id, generation,
                            "outcome repair deferred: the record moved during the read");
                        self.defer(job_id.as_str(), generation).await;
                        continue;
                    }
                    Err(e) => {
                        report.failed += 1;
                        tracing::warn!(job_id = %job_id, error = %e,
                            "outcome repair could not confirm that the record is absent");
                        self.defer(job_id.as_str(), generation).await;
                        continue;
                    }
                }
            }
            let row = record.as_ref().map(|record| outcome_row(&record.body));
            let label = row.as_ref().map(|(outcome, _)| outcome.label());
            match self
                .publish(&job_id, generation, row, Publisher::Repair)
                .await
            {
                Ok(true) => {
                    report.repaired += 1;
                    if let Some(outcome) = label {
                        tracing::info!(job_id = %job_id, generation, outcome,
                            "submission outcome row repaired");
                    } else {
                        tracing::info!(job_id = %job_id, generation,
                            "outcome repair cleared: the job has no record");
                    }
                }
                Ok(false) => {
                    report.deferred += 1;
                    tracing::debug!(job_id = %job_id, generation,
                        "outcome repair deferred: generation moved");
                }
                Err(e) => {
                    report.failed += 1;
                    tracing::warn!(job_id = %job_id, error = %e,
                        "outcome repair could not publish the submission row");
                    self.defer(job_id.as_str(), generation).await;
                }
            }
        }
        tracing::info!(
            attempted = report.attempted,
            repaired = report.repaired,
            deferred = report.deferred,
            failed = report.failed,
            "submission outcome repair pass finished"
        );
        Ok(report)
    }
}

pub(crate) fn is_unreadable_record(e: &anyhow::Error) -> bool {
    e.root_cause().is::<UnreadableObjectError>() || e.root_cause().is::<serde_json::Error>()
}

/// The `outcome` and `run_flags` columns of the row for `body`.
pub(crate) fn outcome_row(body: &serde_json::Value) -> OutcomeRow {
    match RecordedRun::of(body) {
        RecordedRun::Finished(flags) => {
            (SettledOutcome::Finished, Some(flags.into_iter().collect()))
        }
        RecordedRun::Failed => (SettledOutcome::Failed, None),
    }
}

#[async_trait]
impl SubmissionStore for SqlSubmissionStore {
    async fn write_incoming(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let records = Arc::clone(&self.records);
        let id = job_id.clone();
        let body_owned = body.clone();
        self.replace(job_id, body, async move {
            records.write_incoming(&id, &body_owned).await
        })
        .await
    }

    async fn write_incoming_new(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        let (generation, old) = self.invalidate(job_id).await?;
        let store = self.clone();
        let job_id = job_id.clone();
        let body = body.clone();
        // A cancelled caller must not leave an object-store create running
        // without the SQL publication that follows it.
        tokio::spawn(async move {
            let created = match store.records.write_incoming_new(&job_id, &body).await {
                Ok(created) => created,
                Err(e) => {
                    tracing::warn!(job_id = %job_id, generation, error = %e,
                        "body creation stopped before publishing its outcome");
                    store.put_back(&job_id, generation, old).await;
                    return Err(e);
                }
            };
            match created {
                CreateOutcome::Created => {
                    if !store
                        .publish(
                            &job_id,
                            generation,
                            Some(outcome_row(&body)),
                            Publisher::Writer,
                        )
                        .await
                        .inspect_err(|e| {
                            tracing::warn!(job_id = %job_id, generation, error = %e,
                                "outcome row could not be published after a body creation")
                        })?
                    {
                        tracing::warn!(job_id = %job_id, generation,
                            "outcome repair reopened: generation moved during the body write");
                    }
                }
                CreateOutcome::AlreadyExists => store.put_back(&job_id, generation, old).await,
            }
            Ok(created)
        })
        .await
        .context("body creation task ended before publishing its outcome")?
    }

    async fn write_processed(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let records = Arc::clone(&self.records);
        let id = job_id.clone();
        let body_owned = body.clone();
        self.replace(job_id, body, async move {
            records.write_processed(&id, &body_owned).await
        })
        .await
    }

    async fn get_submission(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
        self.records.get_submission(job_id).await
    }

    async fn list_incoming_after(
        &self,
        after: Option<&JobId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        self.records.list_incoming_after(after, limit).await
    }

    async fn mark_processed(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.records.mark_processed(job_id).await
    }

    async fn delete_incoming(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.records.delete_incoming(job_id).await
    }

    async fn find_job(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
        self.records.find_job(job_id).await
    }

    async fn has_record(&self, job_id: &JobId) -> anyhow::Result<bool> {
        let (indexed, pending): (bool, bool) = self
            .db
            .fetch_one("has_record", || {
                sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM submission_outcomes WHERE job_id = $1),
                            EXISTS (SELECT 1 FROM submission_outcome_repairs
                                    WHERE job_id = $1 AND pending)",
                )
                .bind(job_id.as_str())
            })
            .await?;
        if pending {
            tracing::debug!(job_id = %job_id, "record check: a pending job reads its record");
            return self.records.has_record(job_id).await;
        }
        Ok(indexed)
    }

    async fn find_outcomes(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<HashMap<JobId, SettledOutcome>> {
        if job_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let asked: HashMap<&str, &JobId> = job_ids.iter().map(|id| (id.as_str(), id)).collect();
        let ids: Vec<&str> = asked.keys().copied().collect();
        let found: Vec<(String, Option<String>, bool)> = self
            .db
            .fetch_all("find_outcomes", || {
                sqlx::query_as(
                    "SELECT asked.job_id, o.outcome, r.job_id IS NOT NULL
                     FROM unnest($1::text[]) AS asked (job_id)
                     LEFT JOIN submission_outcomes o ON o.job_id = asked.job_id
                     LEFT JOIN submission_outcome_repairs r
                         ON r.job_id = asked.job_id AND r.pending
                     WHERE o.job_id IS NOT NULL OR r.job_id IS NOT NULL",
                )
                .bind(ids.as_slice())
            })
            .await?;
        let (mut outcomes, pending) = found.into_iter().try_fold(
            (HashMap::new(), Vec::new()),
            |(mut outcomes, mut pending), (id, outcome, is_pending)| {
                let Some(job_id) = asked.get(id.as_str()).copied().cloned() else {
                    return anyhow::Ok((outcomes, pending));
                };
                if is_pending {
                    pending.push(job_id);
                } else if let Some(outcome) = outcome {
                    let outcome = outcome
                        .parse()
                        .with_context(|| format!("submission_outcomes row for {job_id}"))?;
                    outcomes.insert(job_id, outcome);
                }
                anyhow::Ok((outcomes, pending))
            },
        )?;
        if !pending.is_empty() {
            tracing::debug!(
                pending = pending.len(),
                "outcome read: pending jobs read their records"
            );
            outcomes.extend(self.records.find_outcomes(&pending).await?);
        }
        Ok(outcomes)
    }

    async fn find_runs(&self, job_ids: &[JobId]) -> anyhow::Result<HashMap<JobId, RecordedRun>> {
        if job_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let Summaries { rows, pending } = self.summaries(job_ids).await?;
        let (mut runs, mut unfilled) = rows.into_iter().fold(
            (HashMap::<JobId, RecordedRun>::new(), Vec::<JobId>::new()),
            |(mut runs, mut unfilled), (job_id, (outcome, run_flags))| {
                match (outcome, run_flags) {
                    (SettledOutcome::Failed, _) => {
                        runs.insert(job_id, RecordedRun::Failed);
                    }
                    (SettledOutcome::Finished, Some(flags)) => {
                        runs.insert(job_id, RecordedRun::Finished(flags.into_iter().collect()));
                    }
                    (SettledOutcome::Finished, None) => unfilled.push(job_id),
                }
                (runs, unfilled)
            },
        );
        if !unfilled.is_empty() {
            unfilled.sort();
            return Err(MissingRunFlagsError {
                count: unfilled.len(),
                first: unfilled
                    .into_iter()
                    .take(MISSING_FLAGS_NAMED)
                    .map(String::from)
                    .collect(),
            }
            .into());
        }
        if !pending.is_empty() {
            tracing::debug!(
                pending = pending.len(),
                "run read: pending jobs read their records"
            );
            runs.extend(self.records.find_runs(&pending).await?);
        }
        Ok(runs)
    }

    async fn repair_outcomes(&self) -> anyhow::Result<OutcomeRepair> {
        self.repair_outcomes_with(REPAIR_MIN_AGE, REPAIR_PAGE).await
    }

    async fn find_in_score_queue(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Option<SubmissionRecord>> {
        self.records.find_in_score_queue(job_id).await
    }

    async fn enqueue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.records.enqueue(stage, job_id, body).await
    }

    async fn list_queue_after(
        &self,
        stage: ScoreQueueStage,
        after: Option<&JobId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        self.records.list_queue_after(stage, after, limit).await
    }

    async fn read_queue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.records.read_queue(stage, job_id).await
    }

    async fn dequeue(&self, stage: ScoreQueueStage, job_id: &JobId) -> anyhow::Result<()> {
        self.records.dequeue(stage, job_id).await
    }

    async fn write_unverified(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.records.write_unverified(client_id, job_id, body).await
    }

    async fn list_unverified_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>> {
        self.records.list_unverified_client(client_id).await
    }

    async fn delete_unverified(&self, client_id: &ClientId, job_id: &JobId) -> anyhow::Result<()> {
        self.records.delete_unverified(client_id, job_id).await
    }

    async fn delete_unverified_client(
        &self,
        client_id: &ClientId,
        dry_run: bool,
    ) -> anyhow::Result<usize> {
        self.records
            .delete_unverified_client(client_id, dry_run)
            .await
    }

    async fn prune_unverified(
        &self,
        older_than: std::time::Duration,
        dry_run: bool,
    ) -> anyhow::Result<PruneSummary> {
        self.records.prune_unverified(older_than, dry_run).await
    }

    async fn write_runtime_output(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        output: &RuntimeOutput,
    ) -> anyhow::Result<()> {
        self.records
            .write_runtime_output(job_id, client_id, output)
            .await
    }

    async fn list_runtime_output(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
        self.records.list_runtime_output(job_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use rstest::rstest;
    use serde_json::json;

    use pipette_store_fs::LocalFsSubmissionStore;

    use super::*;
    use crate::scratch::TestDb;
    use crate::testing::stored_outcome;

    fn store_over(
        test_db: &TestDb,
        dir: &std::path::Path,
    ) -> (SqlSubmissionStore, Arc<dyn SubmissionStore>) {
        let records: Arc<dyn SubmissionStore> =
            Arc::new(LocalFsSubmissionStore::new(dir.to_path_buf()));
        (
            SqlSubmissionStore::new(test_db.db().clone(), Arc::clone(&records)),
            records,
        )
    }

    async fn put_row(db: &SqlDatabase, job_id: &JobId, outcome: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO submission_outcomes (job_id, outcome) VALUES ($1, $2)")
            .bind(job_id.as_str())
            .bind(outcome)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    async fn repair_state(db: &SqlDatabase, job_id: &JobId) -> anyhow::Result<(i64, bool)> {
        Ok(sqlx::query_as(
            "SELECT generation, pending FROM submission_outcome_repairs WHERE job_id = $1",
        )
        .bind(job_id.as_str())
        .fetch_one(db.pool())
        .await?)
    }

    async fn repair_updated_at(
        db: &SqlDatabase,
        job_id: &JobId,
    ) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
        Ok(sqlx::query_scalar(
            "SELECT updated_at FROM submission_outcome_repairs WHERE job_id = $1",
        )
        .bind(job_id.as_str())
        .fetch_one(db.pool())
        .await?)
    }

    /// One repair pass over at most one candidate, with no age limit.
    async fn repair_now(store: &SqlSubmissionStore) -> anyhow::Result<OutcomeRepair> {
        store.repair_outcomes_with(Duration::ZERO, 1).await
    }

    /// One repair pass repairs `job_id` and leaves its row `failed`.
    async fn assert_repairs_to_failed(
        store: &SqlSubmissionStore,
        db: &SqlDatabase,
        job_id: &JobId,
    ) -> anyhow::Result<()> {
        assert_eq!(repair_now(store).await?.repaired, 1);
        assert_eq!(stored_outcome(db, job_id).await?.as_deref(), Some("failed"));
        Ok(())
    }

    fn failure() -> serde_json::Value {
        json!({ "message_type": "failure" })
    }

    /// A wrapped store that answers `has_record`, `find_outcomes`, and
    /// `find_runs` only for the ids in `readable`, so a test proves which jobs
    /// read their records. A read of any other id fails, and so does any other
    /// call.
    struct GuardedRecords {
        inner: Arc<dyn SubmissionStore>,
        readable: HashSet<JobId>,
    }

    impl GuardedRecords {
        fn check(&self, job_ids: &[JobId]) -> anyhow::Result<()> {
            match job_ids.iter().find(|id| !self.readable.contains(*id)) {
                Some(id) => anyhow::bail!("record read of {id}, which has no pending repair"),
                None => Ok(()),
            }
        }
    }

    #[async_trait]
    impl SubmissionStore for GuardedRecords {
        async fn write_incoming(&self, _: &JobId, _: &serde_json::Value) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn write_incoming_new(
            &self,
            _: &JobId,
            _: &serde_json::Value,
        ) -> anyhow::Result<CreateOutcome> {
            anyhow::bail!("unexpected call")
        }

        async fn write_processed(&self, _: &JobId, _: &serde_json::Value) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn get_submission(&self, _: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
            anyhow::bail!("unexpected call")
        }

        async fn list_incoming_after(
            &self,
            _: Option<&JobId>,
            _: NonZeroUsize,
        ) -> anyhow::Result<Vec<JobId>> {
            anyhow::bail!("unexpected call")
        }

        async fn mark_processed(&self, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn delete_incoming(&self, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn find_job(&self, _: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
            anyhow::bail!("unexpected call")
        }

        async fn has_record(&self, job_id: &JobId) -> anyhow::Result<bool> {
            self.check(std::slice::from_ref(job_id))?;
            self.inner.has_record(job_id).await
        }

        async fn find_outcomes(
            &self,
            job_ids: &[JobId],
        ) -> anyhow::Result<HashMap<JobId, SettledOutcome>> {
            self.check(job_ids)?;
            self.inner.find_outcomes(job_ids).await
        }

        async fn find_runs(
            &self,
            job_ids: &[JobId],
        ) -> anyhow::Result<HashMap<JobId, RecordedRun>> {
            self.check(job_ids)?;
            self.inner.find_runs(job_ids).await
        }

        async fn find_in_score_queue(&self, _: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
            anyhow::bail!("unexpected call")
        }

        async fn enqueue(
            &self,
            _: ScoreQueueStage,
            _: &JobId,
            _: &serde_json::Value,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn list_queue_after(
            &self,
            _: ScoreQueueStage,
            _: Option<&JobId>,
            _: NonZeroUsize,
        ) -> anyhow::Result<Vec<JobId>> {
            anyhow::bail!("unexpected call")
        }

        async fn read_queue(
            &self,
            _: ScoreQueueStage,
            _: &JobId,
        ) -> anyhow::Result<Option<serde_json::Value>> {
            anyhow::bail!("unexpected call")
        }

        async fn dequeue(&self, _: ScoreQueueStage, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn write_unverified(
            &self,
            _: &ClientId,
            _: &JobId,
            _: &serde_json::Value,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn list_unverified_client(
            &self,
            _: &ClientId,
        ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>> {
            anyhow::bail!("unexpected call")
        }

        async fn delete_unverified(&self, _: &ClientId, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn delete_unverified_client(&self, _: &ClientId, _: bool) -> anyhow::Result<usize> {
            anyhow::bail!("unexpected call")
        }

        async fn prune_unverified(
            &self,
            _: std::time::Duration,
            _: bool,
        ) -> anyhow::Result<PruneSummary> {
            anyhow::bail!("unexpected call")
        }

        async fn write_runtime_output(
            &self,
            _: &JobId,
            _: &ClientId,
            _: &RuntimeOutput,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn list_runtime_output(
            &self,
            _: &JobId,
        ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
            anyhow::bail!("unexpected call")
        }
    }

    fn guarded(
        db: &SqlDatabase,
        records: Arc<dyn SubmissionStore>,
        readable: HashSet<JobId>,
    ) -> SqlSubmissionStore {
        SqlSubmissionStore::new(
            db.clone(),
            Arc::new(GuardedRecords {
                inner: records,
                readable,
            }),
        )
    }

    enum Seed {
        /// A write through the SQL store, then a spoiled record, so that only
        /// the row can answer.
        Indexed(serde_json::Value),
        RecordOnly(serde_json::Value),
        /// A pending repair, with a record on the wrapped store or none.
        Pending(Option<serde_json::Value>),
        Nothing,
    }

    /// A row answers its job without a read of the record. A job with a
    /// pending repair reads its record. A job with no row and no pending
    /// repair has no record. A request that repeats an id gets one entry.
    #[tokio::test]
    async fn reads_answer_from_rows_and_pending_records() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (writer, records) = store_over(&test_db, &dir.path().join("submissions"));
        let device = json!({
            "message_type": "success",
            "device_os_name": "Android",
            "client_version": "2026.10.05-E925778",
        });
        let flagged = RecordedRun::Finished(BTreeSet::from([
            "client_version:2026.10.05-e925778".to_string(),
            "os:android".to_string(),
        ]));
        let cases = [
            (
                "job-row-finished",
                Seed::Indexed(device.clone()),
                Some(flagged.clone()),
            ),
            (
                "job-row-failed",
                Seed::Indexed(failure()),
                Some(RecordedRun::Failed),
            ),
            (
                "job-row-no-flags",
                Seed::Indexed(json!({ "message_type": "success" })),
                Some(RecordedRun::Finished(BTreeSet::new())),
            ),
            ("job-record-only", Seed::RecordOnly(device.clone()), None),
            (
                "job-pending-finished",
                Seed::Pending(Some(device.clone())),
                Some(flagged),
            ),
            (
                "job-pending-failed",
                Seed::Pending(Some(failure())),
                Some(RecordedRun::Failed),
            ),
            ("job-pending-absent", Seed::Pending(None), None),
            ("job-nothing", Seed::Nothing, None),
        ];
        let mut readable = HashSet::new();
        for (id, seed, _) in &cases {
            let job_id = JobId::try_new(*id)?;
            match seed {
                Seed::Indexed(body) => {
                    writer.write_processed(&job_id, body).await?;
                    pipette_store_testkit::local_fs_spoil_record(dir.path(), &job_id)?;
                }
                Seed::RecordOnly(body) => records.write_processed(&job_id, body).await?,
                Seed::Pending(body) => {
                    if let Some(body) = body {
                        records.write_processed(&job_id, body).await?;
                    }
                    writer.invalidate(&job_id).await?;
                    readable.insert(job_id);
                }
                Seed::Nothing => {}
            }
        }
        let store = guarded(db, records, readable);
        let ids = cases
            .iter()
            .map(|(id, ..)| JobId::try_new(*id))
            .collect::<Result<Vec<_>, _>>()?;
        let runs = cases
            .into_iter()
            .filter_map(|(id, _, run)| Some((id, run?)))
            .map(|(id, run)| anyhow::Ok((JobId::try_new(id)?, run)))
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let outcomes: HashMap<JobId, SettledOutcome> = runs
            .iter()
            .map(|(job_id, run)| {
                let outcome = match run {
                    RecordedRun::Failed => SettledOutcome::Failed,
                    RecordedRun::Finished(_) => SettledOutcome::Finished,
                };
                (job_id.clone(), outcome)
            })
            .collect();

        for job_id in &ids {
            assert_eq!(
                store.has_record(job_id).await?,
                runs.contains_key(job_id),
                "has_record({job_id})"
            );
        }
        let repeated: Vec<JobId> = ids.iter().chain(&ids).cloned().collect();
        assert_eq!(store.find_outcomes(&repeated).await?, outcomes);
        assert_eq!(store.find_runs(&repeated).await?, runs);
        assert!(store.find_outcomes(&[]).await?.is_empty());
        assert!(store.find_runs(&[]).await?.is_empty());
        Ok(())
    }

    /// A `finished` row with no flags answers `has_record` and
    /// `find_outcomes`. Its flags are unknown, so `find_runs` fails and names
    /// the job.
    #[tokio::test]
    async fn a_finished_row_without_flags_fails_a_run_read() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (_, records) = store_over(&test_db, dir.path());
        let store = guarded(db, records, HashSet::new());
        let unfilled = JobId::try_new("job-unfilled")?;
        let failed = JobId::try_new("job-failed")?;
        crate::testing::put_outcome(db, unfilled.as_str(), "finished", None).await?;
        crate::testing::put_outcome(db, failed.as_str(), "failed", None).await?;

        assert!(store.has_record(&unfilled).await?);
        assert_eq!(
            store.find_outcomes(std::slice::from_ref(&unfilled)).await?,
            HashMap::from([(unfilled.clone(), SettledOutcome::Finished)])
        );
        let error = store
            .find_runs(&[unfilled, failed.clone()])
            .await
            .err()
            .context("a run read of a row without flags succeeded")?;
        let missing = error
            .downcast_ref::<MissingRunFlagsError>()
            .context("the error is not a MissingRunFlagsError")?;
        assert_eq!(
            (missing.count, missing.first.as_slice()),
            (1, ["job-unfilled".to_owned()].as_slice())
        );
        assert!(
            error.to_string().contains("db backfill-job-cells"),
            "{error}"
        );
        assert_eq!(
            store.find_runs(std::slice::from_ref(&failed)).await?,
            HashMap::from([(failed, RecordedRun::Failed)])
        );
        Ok(())
    }

    #[tokio::test]
    async fn meets_the_submission_listing_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        pipette_store_testkit::assert_submission_listings_page(&store).await
    }

    /// A write that replaces a body.
    #[derive(Debug, Clone, Copy)]
    enum Replace {
        Incoming,
        Processed,
    }

    impl Replace {
        /// The directory of the wrapped store that this write puts the body in.
        fn dir(self) -> &'static str {
            match self {
                Self::Incoming => "incoming",
                Self::Processed => "processed",
            }
        }

        async fn write(
            self,
            store: &SqlSubmissionStore,
            job_id: &JobId,
            body: &serde_json::Value,
        ) -> anyhow::Result<()> {
            match self {
                Self::Incoming => store.write_incoming(job_id, body).await,
                Self::Processed => store.write_processed(job_id, body).await,
            }
        }
    }

    /// A failed body write leaves no row; a successful one leaves the row
    /// holding that body's outcome.
    #[rstest]
    #[case::incoming(Replace::Incoming)]
    #[case::processed(Replace::Processed)]
    #[tokio::test]
    async fn a_row_follows_its_body(#[case] replace: Replace) -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let job_id = JobId::try_new("job-replaced")?;
        put_row(db, &job_id, "finished").await?;
        let blocker = dir.path().join(replace.dir());
        std::fs::write(&blocker, b"not a directory")?;

        let refused = replace.write(&store, &job_id, &failure()).await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(stored_outcome(db, &job_id).await?, None);
        assert_eq!(repair_state(db, &job_id).await?, (1, true));

        std::fs::remove_file(&blocker)?;
        replace.write(&store, &job_id, &failure()).await?;
        assert!(records.has_record(&job_id).await?);
        assert_eq!(
            stored_outcome(db, &job_id).await?.as_deref(),
            Some("failed")
        );
        assert_eq!(repair_state(db, &job_id).await?, (2, false));
        Ok(())
    }

    /// A resubmission in `incoming/` beside an older body in `processed/`:
    /// the row holds the outcome of the resubmission, and the wrapped store
    /// gives the outcome in `processed/`.
    #[tokio::test]
    async fn a_resubmission_replaces_the_row_of_a_processed_body() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let job_id = JobId::try_new("job-resubmitted")?;
        let asked = std::slice::from_ref(&job_id);
        store
            .write_processed(&job_id, &json!({ "message_type": "success" }))
            .await?;
        store.write_incoming(&job_id, &failure()).await?;

        assert_eq!(
            store.find_outcomes(asked).await?,
            HashMap::from([(job_id.clone(), SettledOutcome::Failed)])
        );
        assert_eq!(
            records.find_outcomes(asked).await?,
            HashMap::from([(job_id.clone(), SettledOutcome::Finished)])
        );
        Ok(())
    }

    /// `write_incoming_new` that creates an `incoming/` body replaces the row
    /// of the body in `processed/`.
    #[tokio::test]
    async fn a_created_incoming_replaces_the_row_of_a_processed_body() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let job_id = JobId::try_new("job-recreated")?;
        store
            .write_processed(&job_id, &json!({ "message_type": "success" }))
            .await?;

        let created = store.write_incoming_new(&job_id, &failure()).await?;
        assert_eq!(created, CreateOutcome::Created);
        assert_eq!(
            stored_outcome(db, &job_id).await?.as_deref(),
            Some("failed")
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_existing_incoming_keeps_its_row() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let job_id = JobId::try_new("job-incumbent")?;

        let first = store.write_incoming_new(&job_id, &failure()).await?;
        assert_eq!(first, CreateOutcome::Created);
        let second = store
            .write_incoming_new(&job_id, &json!({ "message_type": "success" }))
            .await?;
        assert_eq!(second, CreateOutcome::AlreadyExists);
        assert_eq!(
            stored_outcome(db, &job_id).await?.as_deref(),
            Some("failed")
        );
        Ok(())
    }

    /// A create that finds a body returns `AlreadyExists` even if the row
    /// cannot be put back. The job stays pending, and a repair writes the row.
    #[tokio::test]
    async fn an_existing_incoming_survives_a_failed_put_back() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-put-back-refused")?;
        store.write_incoming_new(&id, &failure()).await?;
        sqlx::raw_sql(
            "CREATE FUNCTION refuse_outcome() RETURNS trigger LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'outcome refused'; END $$;
             CREATE TRIGGER refuse_outcome BEFORE INSERT OR UPDATE ON submission_outcomes
                 FOR EACH ROW EXECUTE FUNCTION refuse_outcome();",
        )
        .execute(db.pool())
        .await?;

        let second = store
            .write_incoming_new(&id, &json!({ "message_type": "success" }))
            .await?;
        assert_eq!(second, CreateOutcome::AlreadyExists);
        assert_eq!(stored_outcome(db, &id).await?, None);
        assert_eq!(repair_state(db, &id).await?, (2, true));

        sqlx::query("DROP TRIGGER refuse_outcome ON submission_outcomes")
            .execute(db.pool())
            .await?;
        assert_repairs_to_failed(&store, db, &id).await
    }

    /// A create that fails puts back the row of the older body.
    #[tokio::test]
    async fn a_failed_create_puts_back_the_row() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-create-refused")?;
        store
            .write_processed(&id, &json!({ "message_type": "success" }))
            .await?;
        std::fs::write(dir.path().join("incoming"), b"not a directory")?;

        let refused = store.write_incoming_new(&id, &failure()).await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(stored_outcome(db, &id).await?.as_deref(), Some("finished"));
        assert_eq!(repair_state(db, &id).await?, (2, false));
        Ok(())
    }

    /// A pending job reads its record and fails on the error of that read. A
    /// job with a row, and a job with neither a row nor a pending repair, do
    /// not touch the wrapped store.
    #[tokio::test]
    async fn a_pending_read_error_propagates() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let indexed = JobId::try_new("job-indexed")?;
        store.write_incoming(&indexed, &failure()).await?;
        let pending = JobId::try_new("job-pending")?;
        store.invalidate(&pending).await?;
        let missed = JobId::try_new("job-missed")?;
        std::fs::create_dir_all(dir.path().join("score-queue"))?;
        std::fs::write(dir.path().join("score-queue/to_do"), b"not a directory")?;

        let found = store.has_record(&pending).await;
        assert!(found.is_err(), "{found:?}");
        let outcomes = store
            .find_outcomes(&[indexed.clone(), pending.clone()])
            .await;
        assert!(outcomes.is_err(), "{outcomes:?}");
        let runs = store.find_runs(&[indexed.clone(), pending]).await;
        assert!(runs.is_err(), "{runs:?}");

        assert!(store.has_record(&indexed).await?);
        assert!(!store.has_record(&missed).await?);
        assert_eq!(
            store.find_outcomes(&[indexed.clone(), missed]).await?,
            HashMap::from([(indexed, SettledOutcome::Failed)])
        );
        Ok(())
    }

    /// If the row write fails, the write of the record fails after the body
    /// write succeeds. A read of that job then uses the wrapped store.
    #[tokio::test]
    async fn a_failed_row_write() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        sqlx::raw_sql(
            "CREATE FUNCTION refuse_outcome() RETURNS trigger LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'outcome refused'; END $$;
             CREATE TRIGGER refuse_outcome BEFORE INSERT OR UPDATE ON submission_outcomes
                 FOR EACH ROW EXECUTE FUNCTION refuse_outcome();",
        )
        .execute(db.pool())
        .await?;

        let written = JobId::try_new("job-written")?;
        let refused = store.write_incoming(&written, &failure()).await;
        assert!(refused.is_err(), "{refused:?}");
        assert!(records.has_record(&written).await?);
        let created = JobId::try_new("job-created")?;
        let refused = store.write_incoming_new(&created, &failure()).await;
        assert!(refused.is_err(), "{refused:?}");
        assert!(records.has_record(&created).await?);

        assert_eq!(
            store.find_outcomes(std::slice::from_ref(&written)).await?,
            HashMap::from([(written.clone(), SettledOutcome::Failed)])
        );
        assert_eq!(stored_outcome(db, &written).await?, None);
        assert_eq!(repair_state(db, &written).await?, (1, true));
        sqlx::query("DROP TRIGGER refuse_outcome ON submission_outcomes")
            .execute(db.pool())
            .await?;
        let repair = store.repair_outcomes_with(Duration::ZERO, 500).await?;
        assert_eq!(repair.repaired, 2);
        assert_eq!(
            stored_outcome(db, &written).await?.as_deref(),
            Some("failed")
        );
        assert_eq!(
            stored_outcome(db, &created).await?.as_deref(),
            Some("failed")
        );
        Ok(())
    }

    /// A repair publishes the outcome of the record, or clears the marker with
    /// no row if no record exists. A second pass finds nothing to repair.
    #[rstest]
    #[case::record_present(Some(failure()), Some("failed"))]
    #[case::record_absent(None, None)]
    #[tokio::test]
    async fn repair_publishes_what_the_record_holds(
        #[case] body: Option<serde_json::Value>,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-repair")?;
        let (generation, _) = store.invalidate(&id).await?;
        if let Some(body) = &body {
            records.write_incoming(&id, body).await?;
        }
        assert_eq!(repair_state(db, &id).await?, (generation, true));
        assert_eq!(
            repair_now(&store).await?,
            OutcomeRepair {
                attempted: 1,
                repaired: 1,
                ..OutcomeRepair::default()
            }
        );
        assert_eq!(stored_outcome(db, &id).await?.as_deref(), expected);
        assert_eq!(repair_state(db, &id).await?, (generation, false));
        assert_eq!(repair_now(&store).await?.attempted, 0);
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_record_keeps_its_repair() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-unreadable-repair")?;
        records.write_processed(&id, &failure()).await?;
        store.invalidate(&id).await?;
        std::fs::write(
            dir.path().join(format!("processed/{id}.json.gz")),
            "not gzip",
        )?;
        let before = repair_updated_at(db, &id).await?;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(repair_now(&store).await?.failed, 1);
        assert_eq!(repair_state(db, &id).await?, (1, true));
        assert!(repair_updated_at(db, &id).await? > before);
        assert_eq!(stored_outcome(db, &id).await?, None);
        Ok(())
    }

    /// A wrapped store whose `find_job` misses every record, as for a record
    /// that moves during the read. `has_record` gives `found`, or fails for
    /// `None`. Any other call fails.
    struct MovingRecord {
        found: Option<bool>,
    }

    #[async_trait]
    impl SubmissionStore for MovingRecord {
        async fn write_incoming(&self, _: &JobId, _: &serde_json::Value) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn write_incoming_new(
            &self,
            _: &JobId,
            _: &serde_json::Value,
        ) -> anyhow::Result<CreateOutcome> {
            anyhow::bail!("unexpected call")
        }

        async fn write_processed(&self, _: &JobId, _: &serde_json::Value) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn get_submission(&self, _: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
            anyhow::bail!("unexpected call")
        }

        async fn list_incoming_after(
            &self,
            _: Option<&JobId>,
            _: NonZeroUsize,
        ) -> anyhow::Result<Vec<JobId>> {
            anyhow::bail!("unexpected call")
        }

        async fn mark_processed(&self, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn delete_incoming(&self, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn find_job(&self, _: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
            Ok(None)
        }

        async fn has_record(&self, _: &JobId) -> anyhow::Result<bool> {
            self.found
                .ok_or_else(|| anyhow::anyhow!("record check failed"))
        }

        async fn find_outcomes(
            &self,
            _: &[JobId],
        ) -> anyhow::Result<HashMap<JobId, SettledOutcome>> {
            anyhow::bail!("unexpected call")
        }

        async fn find_runs(&self, _: &[JobId]) -> anyhow::Result<HashMap<JobId, RecordedRun>> {
            anyhow::bail!("unexpected call")
        }

        async fn find_in_score_queue(&self, _: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
            anyhow::bail!("unexpected call")
        }

        async fn enqueue(
            &self,
            _: ScoreQueueStage,
            _: &JobId,
            _: &serde_json::Value,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn list_queue_after(
            &self,
            _: ScoreQueueStage,
            _: Option<&JobId>,
            _: NonZeroUsize,
        ) -> anyhow::Result<Vec<JobId>> {
            anyhow::bail!("unexpected call")
        }

        async fn read_queue(
            &self,
            _: ScoreQueueStage,
            _: &JobId,
        ) -> anyhow::Result<Option<serde_json::Value>> {
            anyhow::bail!("unexpected call")
        }

        async fn dequeue(&self, _: ScoreQueueStage, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn write_unverified(
            &self,
            _: &ClientId,
            _: &JobId,
            _: &serde_json::Value,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn list_unverified_client(
            &self,
            _: &ClientId,
        ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>> {
            anyhow::bail!("unexpected call")
        }

        async fn delete_unverified(&self, _: &ClientId, _: &JobId) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn delete_unverified_client(&self, _: &ClientId, _: bool) -> anyhow::Result<usize> {
            anyhow::bail!("unexpected call")
        }

        async fn prune_unverified(
            &self,
            _: std::time::Duration,
            _: bool,
        ) -> anyhow::Result<PruneSummary> {
            anyhow::bail!("unexpected call")
        }

        async fn write_runtime_output(
            &self,
            _: &JobId,
            _: &ClientId,
            _: &RuntimeOutput,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unexpected call")
        }

        async fn list_runtime_output(
            &self,
            _: &JobId,
        ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
            anyhow::bail!("unexpected call")
        }
    }

    /// A miss in `find_job` clears the repair only if `has_record` confirms
    /// that no record exists.
    #[rstest]
    #[case::moved(Some(true), (0, 1, 0), true)]
    #[case::absent(Some(false), (1, 0, 0), false)]
    #[case::unconfirmed(None, (0, 0, 1), true)]
    #[tokio::test]
    async fn a_missed_record_is_confirmed_before_the_repair_clears(
        #[case] found: Option<bool>,
        #[case] (repaired, deferred, failed): (usize, usize, usize),
        #[case] pending: bool,
    ) -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlSubmissionStore::new(db.clone(), Arc::new(MovingRecord { found }));
        let id = JobId::try_new("job-moving")?;
        store.invalidate(&id).await?;

        assert_eq!(
            repair_now(&store).await?,
            OutcomeRepair {
                attempted: 1,
                repaired,
                deferred,
                failed,
            }
        );
        assert_eq!(repair_state(db, &id).await?, (1, pending));
        assert_eq!(stored_outcome(db, &id).await?, None);
        Ok(())
    }

    /// A repair with an invalid job id fails and moves behind the other pending
    /// repairs, so the next pass reaches them.
    #[tokio::test]
    async fn an_invalid_job_id_does_not_hold_the_page() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        sqlx::query(
            "INSERT INTO submission_outcome_repairs (job_id, generation, pending, updated_at)
             VALUES ('job_invalid', 1, true, now() - interval '1 hour')",
        )
        .execute(db.pool())
        .await?;
        let id = JobId::try_new("job-behind-invalid")?;
        store.invalidate(&id).await?;
        records.write_incoming(&id, &failure()).await?;

        assert_eq!(repair_now(&store).await?.failed, 1);
        assert_repairs_to_failed(&store, db, &id).await
    }

    /// A writer that finds no repair row at publication leaves a pending
    /// repair, so a pruned repair row costs one repair pass.
    #[tokio::test]
    async fn a_publication_without_a_repair_row_reopens_the_repair() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-pruned-repair")?;
        let (generation, _) = store.invalidate(&id).await?;
        records.write_incoming(&id, &failure()).await?;
        sqlx::query("DELETE FROM submission_outcome_repairs WHERE job_id = $1")
            .bind(id.as_str())
            .execute(db.pool())
            .await?;

        assert!(
            !store
                .publish(
                    &id,
                    generation,
                    Some(outcome_row(&failure())),
                    Publisher::Writer
                )
                .await?
        );
        assert_eq!(repair_state(db, &id).await?, (1, true));
        assert_eq!(stored_outcome(db, &id).await?, None);
        assert_repairs_to_failed(&store, db, &id).await
    }

    #[tokio::test]
    async fn a_stale_publication_reopens_the_repair() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-stale")?;
        let (old, _) = store.invalidate(&id).await?;
        let (current, _) = store.invalidate(&id).await?;
        assert!(
            !store
                .publish(&id, old, Some(outcome_row(&failure())), Publisher::Writer)
                .await?
        );
        assert_eq!(repair_state(db, &id).await?, (current + 1, true));
        assert_eq!(stored_outcome(db, &id).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn an_existing_body_without_a_row_remains_pending() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-unindexed-incumbent")?;
        records.write_incoming(&id, &failure()).await?;
        assert_eq!(
            store
                .write_incoming_new(&id, &json!({"message_type": "success"}))
                .await?,
            CreateOutcome::AlreadyExists
        );
        assert_eq!(repair_state(db, &id).await?, (1, true));
        assert_eq!(stored_outcome(db, &id).await?, None);
        assert_repairs_to_failed(&store, db, &id).await
    }

    #[tokio::test]
    async fn repair_respects_age_limit_and_pending_publication() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let first = JobId::try_new("job-page-1")?;
        let second = JobId::try_new("job-page-2")?;
        for id in [&first, &second] {
            store.invalidate(id).await?;
            records.write_incoming(id, &failure()).await?;
        }
        assert_eq!(
            store
                .repair_outcomes_with(REPAIR_MIN_AGE, 1)
                .await?
                .attempted,
            0
        );
        assert_eq!(repair_now(&store).await?.attempted, 1);
        assert_eq!(repair_now(&store).await?.attempted, 1);
        assert_eq!(repair_now(&store).await?.attempted, 0);
        assert!(
            !store
                .publish(&first, 1, Some(outcome_row(&failure())), Publisher::Repair)
                .await?
        );
        assert_eq!(repair_state(db, &first).await?, (1, false));
        Ok(())
    }

    #[tokio::test]
    async fn a_cancelled_write_leaves_repair_work() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let records: Arc<dyn SubmissionStore> =
            Arc::new(LocalFsSubmissionStore::new(dir.path().to_path_buf()));
        let one_connection = SqlDatabase::from_options((*db.pool().connect_options()).clone(), 1);
        let store = SqlSubmissionStore::new(one_connection, Arc::clone(&records));
        let id = JobId::try_new("job-cancelled")?;
        let body = failure();
        records
            .write_incoming(&id, &body)
            .await
            .context("seed body")?;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let task_store = store.clone();
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            let write_db = task_store.db.clone();
            task_store
                .replace(&task_id, &body, async move {
                    sqlx::query("SELECT 1").execute(write_db.pool()).await?;
                    let _ = entered_tx.send(());
                    let _ = release_rx.await;
                    Ok(())
                })
                .await
        });
        entered_rx.await.context("enter write")?;
        task.abort();
        let _ = task.await;
        assert_eq!(
            repair_state(db, &id).await.context("read pending")?,
            (1, true)
        );
        assert_eq!(stored_outcome(db, &id).await.context("read outcome")?, None);
        assert_repairs_to_failed(&store, db, &id)
            .await
            .context("repair")
    }

    #[tokio::test]
    async fn a_late_write_publishes_after_repair_clears_absence() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-late-after-absence")?;
        let body = failure();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let task_store = store.clone();
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            let write_id = task_id.clone();
            let write_body = body.clone();
            task_store
                .replace(&task_id, &body, async move {
                    let _ = entered_tx.send(());
                    release_rx.await?;
                    records.write_incoming(&write_id, &write_body).await
                })
                .await
        });
        entered_rx.await?;
        task.abort();
        let _ = task.await;
        assert_eq!(repair_now(&store).await?.repaired, 1);
        assert_eq!(repair_state(db, &id).await?, (1, false));
        assert_eq!(stored_outcome(db, &id).await?, None);

        release_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("body writer stopped after cancellation"))?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if stored_outcome(db, &id).await?.as_deref() == Some("failed") {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert!(store.has_record(&id).await?);
        Ok(())
    }

    /// A repair pass that runs during a body write publishes the older record.
    /// The writer then publishes its own body for the same generation, so the
    /// row holds the newer outcome. The pass uses the one connection of the
    /// pool while the write is open, so the writer holds no connection there.
    #[tokio::test]
    async fn a_repair_during_a_body_write_yields_to_the_writer() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let records: Arc<dyn SubmissionStore> =
            Arc::new(LocalFsSubmissionStore::new(dir.path().to_path_buf()));
        let one_connection = SqlDatabase::from_options((*db.pool().connect_options()).clone(), 1);
        let store = SqlSubmissionStore::new(one_connection, Arc::clone(&records));
        let id = JobId::try_new("job-repair-during-write")?;
        records
            .write_incoming(&id, &json!({ "message_type": "success" }))
            .await?;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let writer_store = store.clone();
        let writer_records = Arc::clone(&records);
        let writer_id = id.clone();
        let writer = tokio::spawn(async move {
            let body = failure();
            let write_id = writer_id.clone();
            let write_body = body.clone();
            writer_store
                .replace(&writer_id, &body, async move {
                    let _ = entered_tx.send(());
                    release_rx.await?;
                    writer_records.write_incoming(&write_id, &write_body).await
                })
                .await
        });
        entered_rx.await.context("enter write")?;

        let repair = tokio::time::timeout(Duration::from_secs(10), repair_now(&store))
            .await
            .context("the repair pass waited for a connection")??;
        assert_eq!(repair.repaired, 1);
        assert_eq!(stored_outcome(db, &id).await?.as_deref(), Some("finished"));
        assert_eq!(repair_state(db, &id).await?, (1, false));

        release_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("writer stopped"))?;
        writer.await??;
        assert_eq!(stored_outcome(db, &id).await?.as_deref(), Some("failed"));
        assert_eq!(repair_state(db, &id).await?, (1, false));
        Ok(())
    }

    #[tokio::test]
    async fn a_late_body_reopens_repair_after_another_writer_publishes() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let id = JobId::try_new("job-late-body")?;
        let body = failure();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let first_store = store.clone();
        let first_records = Arc::clone(&records);
        let first_id = id.clone();
        let first = tokio::spawn(async move {
            let write_id = first_id.clone();
            let write_body = body.clone();
            first_store
                .replace(&first_id, &body, async move {
                    let _ = entered_tx.send(());
                    release_rx.await?;
                    first_records.write_incoming(&write_id, &write_body).await
                })
                .await
        });
        entered_rx.await?;
        store
            .write_incoming(&id, &json!({"message_type": "success"}))
            .await?;
        release_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("writer stopped"))?;
        first.await??;
        assert_eq!(repair_state(db, &id).await?, (3, true));
        assert_eq!(stored_outcome(db, &id).await?, None);
        assert_repairs_to_failed(&store, db, &id).await
    }

    async fn stored_run_flags(
        db: &SqlDatabase,
        job_id: &JobId,
    ) -> anyhow::Result<Option<Vec<String>>> {
        Ok(
            sqlx::query_scalar("SELECT run_flags FROM submission_outcomes WHERE job_id = $1")
                .bind(job_id.as_str())
                .fetch_one(db.pool())
                .await?,
        )
    }

    /// A finished row holds the flags that its record proves the run held. A
    /// failed row holds no flags. A rewrite replaces the flags.
    #[rstest]
    #[case::incoming(Replace::Incoming)]
    #[case::processed(Replace::Processed)]
    #[tokio::test]
    async fn a_row_holds_the_flags_of_a_finished_run(
        #[case] replace: Replace,
    ) -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let job_id = JobId::try_new("job-flagged")?;
        let success = |os: &str| {
            json!({
                "message_type": "success",
                "device_os_name": os,
                "client_version": "2026.10.05-E925778",
            })
        };

        replace.write(&store, &job_id, &success("Android")).await?;
        assert_eq!(
            stored_run_flags(db, &job_id).await?,
            Some(vec![
                "client_version:2026.10.05-e925778".to_string(),
                "os:android".to_string(),
            ])
        );
        replace.write(&store, &job_id, &success("iOS")).await?;
        assert_eq!(
            stored_run_flags(db, &job_id).await?,
            Some(vec![
                "client_version:2026.10.05-e925778".to_string(),
                "os:ios".to_string(),
            ])
        );
        replace.write(&store, &job_id, &failure()).await?;
        assert_eq!(stored_run_flags(db, &job_id).await?, None);

        let created = JobId::try_new("job-created-flagged")?;
        store
            .write_incoming_new(&created, &json!({ "message_type": "success" }))
            .await?;
        assert_eq!(stored_run_flags(db, &created).await?, Some(Vec::new()));
        Ok(())
    }
}
