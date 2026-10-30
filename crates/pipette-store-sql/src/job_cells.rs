//! The backfill of the cell index. It indexes the jobs that ingestion minted
//! before the index existed. It writes a `job_cells` row for each job source,
//! and the `run_flags` of each finished run.

use futures::stream::{StreamExt, TryStreamExt};

use pipette_core::plan::SettledOutcome;
use pipette_plan_types::JobId;
use pipette_store::{SubmissionStore, UnreadableObjectError};

use crate::db::SqlDatabase;
use crate::submissions::outcome_row;

/// The rows that one statement of the backfill reads.
const PAGE: i64 = 500;

/// The submission records that the backfill reads at one time. A record can be
/// tens of MB.
const RECORD_READS: usize = 8;

/// What [`backfill_job_cells`] wrote, or would write on a dry run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JobCellBackfill {
    /// Job sources given a `job_cells` row.
    pub cells_written: usize,
    /// `submission_outcomes` rows written or given their `run_flags`.
    pub outcomes_written: usize,
    /// Job sources with no submission record: the job is unfinished, or its
    /// record was pruned.
    pub without_record: usize,
    /// Job sources whose body does not parse.
    pub unreadable_sources: usize,
    /// Submission records that do not parse.
    pub unreadable_records: usize,
}

/// Index every job source that has no `job_cells` row, with the digest that
/// `cell_of` gives. Then write the outcome row of each job that has no row, or
/// whose row is `finished` and has no `run_flags`. The row comes from the
/// record of the job in `records`. A second run writes only the rows that are
/// still missing. The backfill counts and skips a record or a body that does
/// not parse. Any other failed read stops the backfill.
pub async fn backfill_job_cells(
    db: &SqlDatabase,
    records: &dyn SubmissionStore,
    cell_of: &(dyn Fn(&serde_json::Value) -> String + Sync),
    dry_run: bool,
) -> anyhow::Result<JobCellBackfill> {
    let mut report = JobCellBackfill::default();
    backfill_cells(db, cell_of, dry_run, &mut report).await?;
    backfill_outcomes(db, records, dry_run, &mut report).await?;
    tracing::info!(
        dry_run,
        cells_written = report.cells_written,
        outcomes_written = report.outcomes_written,
        without_record = report.without_record,
        unreadable_sources = report.unreadable_sources,
        unreadable_records = report.unreadable_records,
        "job cell index backfilled"
    );
    Ok(report)
}

async fn backfill_cells(
    db: &SqlDatabase,
    cell_of: &(dyn Fn(&serde_json::Value) -> String + Sync),
    dry_run: bool,
    report: &mut JobCellBackfill,
) -> anyhow::Result<()> {
    let mut after = String::new();
    loop {
        let page: Vec<(String, String)> = db
            .fetch_all("backfill_job_cells_page", || {
                sqlx::query_as(
                    "SELECT s.job_id, s.body FROM job_sources s
                     WHERE s.job_id > $1
                       AND NOT EXISTS (SELECT 1 FROM job_cells c WHERE c.job_id = s.job_id)
                     ORDER BY s.job_id
                     LIMIT $2",
                )
                .bind(after.as_str())
                .bind(PAGE)
            })
            .await?;
        let Some((last, _)) = page.last() else {
            return Ok(());
        };
        after = last.clone();
        for (job_id, body) in page {
            let body: serde_json::Value = match serde_json::from_str(&body) {
                Ok(body) => body,
                Err(e) => {
                    tracing::warn!(
                        job_id = %job_id,
                        error = %e,
                        "job cell backfill skipped a job source that does not parse"
                    );
                    report.unreadable_sources += 1;
                    continue;
                }
            };
            if !dry_run {
                // The source does not say whose campaign minted the job, so
                // the row has no scope and answers no later plan.
                insert_job_cell(db, &job_id, &cell_of(&body), None).await?;
            }
            report.cells_written += 1;
        }
    }
}

/// Write the cell of `job_id` and the scope that may reuse its run. A job
/// keeps the first row written for it. A row with no scope answers no plan.
pub(crate) async fn insert_job_cell(
    db: &SqlDatabase,
    job_id: &str,
    cell_digest: &str,
    scope: Option<&str>,
) -> anyhow::Result<()> {
    db.execute("put_job_cell", || {
        sqlx::query(
            "INSERT INTO job_cells (job_id, cell_digest, scope) VALUES ($1, $2, $3)
             ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(job_id)
        .bind(cell_digest)
        .bind(scope)
    })
    .await?;
    Ok(())
}

/// What the record of one job gave the outcome backfill. A page holds the
/// outcome row of each record, and not the record, because a record can be
/// tens of MB.
enum RecordRead {
    Row(JobId, SettledOutcome, Option<Vec<String>>),
    Absent,
    Unreadable,
}

async fn backfill_outcomes(
    db: &SqlDatabase,
    records: &dyn SubmissionStore,
    dry_run: bool,
    report: &mut JobCellBackfill,
) -> anyhow::Result<()> {
    let mut after = String::new();
    loop {
        let page: Vec<(String,)> = db
            .fetch_all("backfill_outcomes_page", || {
                sqlx::query_as(
                    "SELECT s.job_id FROM job_sources s
                     LEFT JOIN submission_outcomes o ON o.job_id = s.job_id
                     WHERE s.job_id > $1
                       AND (o.job_id IS NULL OR (o.outcome = 'finished' AND o.run_flags IS NULL))
                     ORDER BY s.job_id
                     LIMIT $2",
                )
                .bind(after.as_str())
                .bind(PAGE)
            })
            .await?;
        let Some((last,)) = page.last() else {
            return Ok(());
        };
        after = last.clone();
        let reads: Vec<RecordRead> = futures::stream::iter(page)
            .map(|(job_id,)| read_record(records, job_id))
            .buffer_unordered(RECORD_READS)
            .try_collect()
            .await?;
        for read in reads {
            match read {
                RecordRead::Absent => report.without_record += 1,
                RecordRead::Unreadable => report.unreadable_records += 1,
                RecordRead::Row(job_id, outcome, run_flags) => {
                    if !dry_run {
                        write_backfilled_outcome(db, &job_id, outcome, run_flags.as_deref())
                            .await?;
                    }
                    report.outcomes_written += 1;
                }
            }
        }
    }
}

async fn read_record(records: &dyn SubmissionStore, job_id: String) -> anyhow::Result<RecordRead> {
    let Ok(parsed) = JobId::try_new(&job_id) else {
        tracing::warn!(key = %job_id, "job cell backfill skipped an unparseable job-sources key");
        return Ok(RecordRead::Unreadable);
    };
    match records.find_job(&parsed).await {
        Ok(Some(record)) => {
            let (outcome, run_flags) = outcome_row(&record.body);
            Ok(RecordRead::Row(parsed, outcome, run_flags))
        }
        Ok(None) => Ok(RecordRead::Absent),
        // The object stores give a bare `serde_json::Error` for a record that
        // does not parse, gzip damage included.
        Err(e)
            if e.root_cause().is::<UnreadableObjectError>()
                || e.root_cause().is::<serde_json::Error>() =>
        {
            tracing::warn!(
                job_id = %parsed,
                error = %e,
                "job cell backfill could not read a submission record"
            );
            Ok(RecordRead::Unreadable)
        }
        Err(e) => Err(e.context(format!("reading the submission record of job {parsed}"))),
    }
}

/// Write the row of `job_id`. Keep a complete row that a submission write put
/// there after the backfill read the page.
async fn write_backfilled_outcome(
    db: &SqlDatabase,
    job_id: &JobId,
    outcome: SettledOutcome,
    run_flags: Option<&[String]>,
) -> anyhow::Result<()> {
    db.execute("backfill_outcome", || {
        sqlx::query(
            "INSERT INTO submission_outcomes (job_id, outcome, run_flags) VALUES ($1, $2, $3)
             ON CONFLICT (job_id) DO UPDATE
                 SET outcome = EXCLUDED.outcome, run_flags = EXCLUDED.run_flags
                 WHERE submission_outcomes.outcome = 'finished'
                   AND submission_outcomes.run_flags IS NULL",
        )
        .bind(job_id.as_str())
        .bind(outcome.label())
        .bind(run_flags)
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use pipette_store_fs::LocalFsSubmissionStore;

    use super::*;
    use crate::testing::put_outcome;

    async fn put_source(db: &SqlDatabase, job_id: &str, body: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO job_sources (job_id, body) VALUES ($1, $2)")
            .bind(job_id)
            .bind(body)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    type Cells = Vec<(String, String)>;
    type Outcomes = Vec<(String, String, Option<Vec<String>>)>;

    async fn index(db: &SqlDatabase) -> anyhow::Result<(Cells, Outcomes)> {
        let cells = sqlx::query_as("SELECT job_id, cell_digest FROM job_cells ORDER BY job_id")
            .fetch_all(db.pool())
            .await?;
        let outcomes = sqlx::query_as(
            "SELECT job_id, outcome, run_flags FROM submission_outcomes ORDER BY job_id",
        )
        .fetch_all(db.pool())
        .await?;
        Ok((cells, outcomes))
    }

    /// A dry run writes nothing. A run indexes every source that parses. It fills
    /// each row that is missing or has no flags from its record, and keeps a
    /// complete row. It counts and skips a record that does not parse. A second
    /// run writes nothing.
    #[tokio::test]
    async fn the_backfill_writes_only_what_is_missing() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let records = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        let android = json!({ "message_type": "success", "device_os_name": "Android" });
        for (job_id, spec) in [
            ("job-1", 1),
            ("job-2", 2),
            ("job-3", 3),
            ("job-5", 5),
            ("job-6", 6),
            ("job-7", 7),
        ] {
            put_source(db, job_id, &json!({ "spec": spec }).to_string()).await?;
        }
        put_source(db, "job-4", "not json").await?;
        records
            .write_processed(&JobId::try_new("job-1")?, &android)
            .await?;
        records
            .write_processed(
                &JobId::try_new("job-2")?,
                &json!({ "message_type": "failure" }),
            )
            .await?;
        records
            .write_processed(&JobId::try_new("job-5")?, &android)
            .await?;
        records
            .write_processed(&JobId::try_new("job-6")?, &android)
            .await?;
        let processed = dir.path().join("processed");
        std::fs::create_dir_all(&processed)?;
        std::fs::write(processed.join("job-7.json.gz"), "not gzip")?;
        put_outcome(db, "job-5", "finished", None).await?;
        put_outcome(db, "job-6", "finished", Some(&["os:ios"])).await?;
        let cell_of = |body: &serde_json::Value| format!("cell-{}", body["spec"]);
        let before = index(db).await?;

        let dry = backfill_job_cells(db, &records, &cell_of, true).await?;
        let expected = JobCellBackfill {
            cells_written: 6,
            outcomes_written: 3,
            without_record: 2,
            unreadable_sources: 1,
            unreadable_records: 1,
        };
        assert_eq!(dry, expected);
        assert_eq!(index(db).await?, before);

        assert_eq!(
            backfill_job_cells(db, &records, &cell_of, false).await?,
            expected
        );
        let flags = |flags: &[&str]| Some(flags.iter().map(|f| f.to_string()).collect());
        let cell = |job_id: &str, n: u8| (job_id.to_string(), format!("cell-{n}"));
        let outcome = |job_id: &str, outcome: &str, run_flags| {
            (job_id.to_string(), outcome.to_string(), run_flags)
        };
        assert_eq!(
            index(db).await?,
            (
                vec![
                    cell("job-1", 1),
                    cell("job-2", 2),
                    cell("job-3", 3),
                    cell("job-5", 5),
                    cell("job-6", 6),
                    cell("job-7", 7),
                ],
                vec![
                    outcome("job-1", "finished", flags(&["os:android"])),
                    outcome("job-2", "failed", None),
                    outcome("job-5", "finished", flags(&["os:android"])),
                    outcome("job-6", "finished", flags(&["os:ios"])),
                ],
            )
        );

        assert_eq!(
            backfill_job_cells(db, &records, &cell_of, false).await?,
            JobCellBackfill {
                without_record: 2,
                unreadable_sources: 1,
                unreadable_records: 1,
                ..JobCellBackfill::default()
            }
        );
        Ok(())
    }
}
