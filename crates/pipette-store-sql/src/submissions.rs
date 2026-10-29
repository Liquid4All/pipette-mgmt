//! [`SubmissionStore`] with an index of record outcomes on SQL and the records
//! on the wrapped object store. A read with no row falls back to that store and
//! writes no row; `docs/storage.md` gives the row rules and remaining windows.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use pipette_core::client::recorded_run_flags;
use pipette_core::plan::SettledOutcome;
use pipette_plan_types::{ClientId, JobId, RuntimeOutput};
use pipette_store::{
    CreateOutcome, PruneSummary, ScoreQueueStage, SubmissionRecord, SubmissionStore,
};

use crate::db::SqlDatabase;

#[derive(Clone)]
pub(crate) struct SqlSubmissionStore {
    db: SqlDatabase,
    records: Arc<dyn SubmissionStore>,
}

impl SqlSubmissionStore {
    pub(crate) fn new(db: SqlDatabase, records: Arc<dyn SubmissionStore>) -> Self {
        Self { db, records }
    }

    /// Delete the row of `job_id`, before a write replaces its body.
    async fn forget_outcome(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.db
            .execute("forget_outcome", || {
                sqlx::query("DELETE FROM submission_outcomes WHERE job_id = $1")
                    .bind(job_id.as_str())
            })
            .await
            .inspect_err(|e| {
                tracing::warn!(
                    job_id = %job_id,
                    error = %e,
                    "submission outcome row delete failed before a body write"
                );
            })?;
        Ok(())
    }

    /// Write the row of `job_id`, after a write puts its body. A finished
    /// body also gives the flags that its run held. A failed body gives no
    /// flags.
    async fn record_outcome(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let (outcome, run_flags) = outcome_row(body);
        self.db
            .execute("record_outcome", || {
                sqlx::query(
                    "INSERT INTO submission_outcomes (job_id, outcome, run_flags) VALUES ($1, $2, $3)
                     ON CONFLICT (job_id) DO UPDATE
                         SET outcome = EXCLUDED.outcome, run_flags = EXCLUDED.run_flags",
                )
                .bind(job_id.as_str())
                .bind(outcome.label())
                .bind(run_flags.as_deref())
            })
            .await
            .inspect_err(|e| {
                tracing::warn!(
                    job_id = %job_id,
                    error = %e,
                    "submission outcome row write failed after a body write"
                );
            })?;
        Ok(())
    }

    /// Delete the row of `job_id`, run `write` to replace its body with
    /// `body`, then write the row.
    async fn replace(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
        write: impl Future<Output = anyhow::Result<()>> + Send,
    ) -> anyhow::Result<()> {
        self.forget_outcome(job_id).await?;
        write.await?;
        self.record_outcome(job_id, body).await
    }
}

/// The outcome of a body, by the rule that [`pipette_store::OutcomeProbe`]
/// applies to a stored record.
fn outcome_of(body: &serde_json::Value) -> SettledOutcome {
    SettledOutcome::from_message_type(body.get("message_type").and_then(serde_json::Value::as_str))
}

/// The `outcome` and `run_flags` columns of the row for `body`. Only a finished
/// run gives flags.
pub(crate) fn outcome_row(body: &serde_json::Value) -> (SettledOutcome, Option<Vec<String>>) {
    let outcome = outcome_of(body);
    let run_flags = (outcome == SettledOutcome::Finished)
        .then(|| recorded_run_flags(body).into_iter().collect());
    (outcome, run_flags)
}

#[async_trait]
impl SubmissionStore for SqlSubmissionStore {
    async fn write_incoming(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        self.replace(job_id, body, self.records.write_incoming(job_id, body))
            .await
    }

    async fn write_incoming_new(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        // The body of the incumbent stays, and so does its row.
        let created = self.records.write_incoming_new(job_id, body).await?;
        if created == CreateOutcome::Created {
            self.record_outcome(job_id, body).await?;
        }
        Ok(created)
    }

    async fn write_processed(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.replace(job_id, body, self.records.write_processed(job_id, body))
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
        let (indexed,): (bool,) = self
            .db
            .fetch_one("has_record", || {
                sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM submission_outcomes WHERE job_id = $1)",
                )
                .bind(job_id.as_str())
            })
            .await?;
        if indexed {
            return Ok(true);
        }
        self.records.has_record(job_id).await
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
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("find_outcomes", || {
                sqlx::query_as(
                    "SELECT job_id, outcome FROM submission_outcomes WHERE job_id = ANY($1)",
                )
                .bind(ids.as_slice())
            })
            .await?;
        let mut outcomes: HashMap<JobId, SettledOutcome> = rows
            .into_iter()
            // The statement answers only the ids it was given.
            .filter_map(|(job_id, outcome)| Some(((*asked.get(job_id.as_str())?).clone(), outcome)))
            .map(|(job_id, outcome)| {
                let outcome = outcome
                    .parse()
                    .with_context(|| format!("submission_outcomes row for {job_id}"))?;
                anyhow::Ok((job_id, outcome))
            })
            .collect::<anyhow::Result<_>>()?;
        let misses: Vec<JobId> = asked
            .into_values()
            .filter(|job_id| !outcomes.contains_key(*job_id))
            .cloned()
            .collect();
        if misses.is_empty() {
            return Ok(outcomes);
        }
        outcomes.extend(self.records.find_outcomes(&misses).await?);
        Ok(outcomes)
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

    fn failure() -> serde_json::Value {
        json!({ "message_type": "failure" })
    }

    #[tokio::test]
    async fn meets_the_submission_store_contracts() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        pipette_store_testkit::assert_has_record_contract(&store).await?;
        pipette_store_testkit::assert_find_outcomes_contract(&store).await
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

        std::fs::remove_file(&blocker)?;
        replace.write(&store, &job_id, &failure()).await?;
        assert!(records.has_record(&job_id).await?);
        assert_eq!(
            stored_outcome(db, &job_id).await?.as_deref(),
            Some("failed")
        );
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

    /// A record with no row reads from the wrapped store, and the read writes
    /// no row.
    #[tokio::test]
    async fn a_miss_falls_back() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, records) = store_over(&test_db, dir.path());
        let job_id = JobId::try_new("job-unindexed")?;
        let absent = JobId::try_new("job-absent")?;
        records.write_processed(&job_id, &failure()).await?;

        assert!(store.has_record(&job_id).await?);
        assert_eq!(
            stored_outcome(db, &job_id).await?,
            None,
            "a HEAD reads no outcome"
        );

        let asked = [job_id.clone(), absent.clone()];
        let expected = HashMap::from([(job_id.clone(), SettledOutcome::Failed)]);
        assert_eq!(store.find_outcomes(&asked).await?, expected);
        assert_eq!(stored_outcome(db, &job_id).await?, None);
        assert_eq!(stored_outcome(db, &absent).await?, None);
        Ok(())
    }

    /// With no row, the read uses the wrapped store and fails on its error;
    /// with a row, it does not touch the wrapped store.
    #[tokio::test]
    async fn a_fallback_read_error_propagates() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = store_over(&test_db, dir.path());
        let indexed = JobId::try_new("job-indexed")?;
        store.write_incoming(&indexed, &failure()).await?;
        std::fs::create_dir_all(dir.path().join("score-queue"))?;
        std::fs::write(dir.path().join("score-queue/to_do"), b"not a directory")?;

        let missed = JobId::try_new("job-missed")?;
        let found = store.has_record(&missed).await;
        assert!(found.is_err(), "{found:?}");
        let outcomes = store.find_outcomes(&[indexed.clone(), missed]).await;
        assert!(outcomes.is_err(), "{outcomes:?}");

        assert!(store.has_record(&indexed).await?);
        assert_eq!(
            store.find_outcomes(std::slice::from_ref(&indexed)).await?,
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
        Ok(())
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
