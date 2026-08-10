//! `requeue-eval` subcommand: re-stage already-scored eval submissions
//! back into `submissions/incoming/` so the next scoring passes
//! (`process-submissions` routes them, `score-eval` re-scores) handle them
//! again as brand-new submissions.
//!
//! Why: scoring outcomes are written once, when a submission is first
//! scored, and the processing lifecycle keys state on file location — a
//! body in `submissions/processed/` is never re-scored. So when the
//! scorer's logic changes (e.g. an IFBench scoring fix), already-scored
//! submissions keep their stale verdicts. The way to get a fresh verdict
//! is to put a copy back in `incoming/`.
//!
//! ## How jobs are selected
//!
//! The caller names a benchmark id (e.g. `eval_ifbench_original`). It is
//! looked up in the **configured catalog** and the run errors unless it
//! exists and is an `eval`. Selection then runs off the **warehouse
//! metrics**, not the submission bodies: a single pass over the warehouse
//! — the same [`WarehouseStore::for_each_metric_row`] facility the `fix-*`
//! commands use — collects the jobs whose rows carry that `benchmark_id`.
//! Each metric `result_id` is `{job_id}_{i}`; we collect the distinct
//! `job_id`s.
//!
//! ## How each body is re-staged
//!
//! For each job we do the submit handler's job: take the wire input, mint
//! a fresh `job_id` and a `submitted_at = now`, attach the original
//! `client_id` and the catalog-resolved `benchmark_type` via
//! `into_submission`, and serialize exactly as the handler writes
//! `incoming/`. The re-staged submission is treated as brand-new — the
//! next `score` run writes new warehouse rows (partitioned by today's
//! `month=`) and a new `processed/{new_job_id}` archive. The original
//! `job_id`'s rows and archive are left untouched: old verdicts stay
//! readable alongside the fresh ones, distinguishable by `submitted_at`
//! and `job_id`.
//!
//! Race: this writes into `incoming/`, which `score` consumes, and reads
//! the warehouse. The storage mutate lock (see [`crate::storage_lock`])
//! serializes it against `score` and the `fix-*` commands.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, TryStreamExt, stream};

use crate::benchmark::{Benchmark, BenchmarkDef, BenchmarkType};
use crate::stores::Stores;
use crate::submission::{Submission, SubmissionInput, parse_stored_submission};
use crate::types::{BenchmarkId, JobId};
use crate::warehouse::MetricRow;

const HEARTBEAT_INTERVAL: usize = 1000;

/// Optional constraints narrowing which warehouse jobs are re-staged, all
/// AND-combined; a `None` field places no constraint on that axis.
///
/// Because re-staging is non-idempotent (each run mints a fresh `job_id`
/// and `submitted_at = now`), re-running over the whole benchmark doubles
/// the re-staged set every time. `submitted_before` is the dependable way
/// to avoid that: set it to a moment before the migration started and the
/// freshly re-staged copies — whose `submitted_at` is "now" — fall outside
/// the window, so only the original jobs are ever re-staged.
#[derive(Clone, Default)]
pub struct Filters {
    /// Inclusive lower bound on the row's `submitted_at` (micros since epoch).
    pub submitted_after: Option<i64>,
    /// Inclusive upper bound on the row's `submitted_at` (micros since epoch).
    pub submitted_before: Option<i64>,
    /// Exact match on the row's `score_runtime_version`. For evals this is
    /// the client's on-device runtime version (see `score.rs`), not a
    /// scoring-service version.
    pub score_runtime_version: Option<String>,
}

impl Filters {
    /// Whether a warehouse row clears every active constraint.
    fn matches(&self, row: &MetricRow) -> bool {
        self.submitted_after.is_none_or(|t| row.submitted_at >= t)
            && self.submitted_before.is_none_or(|t| row.submitted_at <= t)
            && self
                .score_runtime_version
                .as_deref()
                .is_none_or(|v| row.score_runtime_version.as_deref() == Some(v))
    }

    /// Whether any constraint is active (used only for log/summary wording).
    fn is_active(&self) -> bool {
        self.submitted_after.is_some()
            || self.submitted_before.is_some()
            || self.score_runtime_version.is_some()
    }
}

/// Per-job result, tallied into the run summary.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Restaged,
    /// Warehouse references a job whose submission body is gone.
    MissingBody,
    /// Body does not parse as a submission.
    Unparseable,
    /// Body is a `message_type: "failure"` submission (not scorable).
    NonSuccess,
}

/// Extract the `job_id` from a metric `result_id` of the form
/// `{job_id}_{i}`. `job_id`s are UUIDs (no underscore), so stripping the
/// trailing `_{i}` recovers it; a `result_id` with no underscore is
/// treated as the whole id.
fn job_id_from_result_id(result_id: &str) -> Option<JobId> {
    let jid = result_id
        .rsplit_once('_')
        .map(|(j, _)| j)
        .unwrap_or(result_id);
    // `try_new` rejects the empty string and any unsafe char, so a malformed
    // result id yields `None` (skipped) rather than a dangerous JobId.
    JobId::try_new(jid).ok()
}

/// Reconstruct one job's canonical submission and, unless `dry_run`, write
/// it into `incoming/` under a fresh `job_id` and `submitted_at = now`.
/// `benchmark_type` is the type resolved once from the catalog for the
/// requested benchmark (all selected jobs share it).
async fn restage_one(
    stores: &Stores,
    benchmark_type: BenchmarkType,
    job_id: &JobId,
    dry_run: bool,
) -> anyhow::Result<Outcome> {
    let Some(record) = stores.submissions.get_submission(job_id).await? else {
        tracing::warn!(job_id = %job_id, "requeue-eval: no submission body found, skipping");
        return Ok(Outcome::MissingBody);
    };

    // Recover the typed submission (tolerates legacy bodies that lack
    // `message_type`; validates shape).
    let submission = match parse_stored_submission(&record.body) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(job_id = %job_id, error = %e, "requeue-eval: body does not parse, skipping");
            return Ok(Outcome::Unparseable);
        }
    };
    let Submission::Success(success) = submission else {
        // Failure bodies aren't scored (and never reach the warehouse), so
        // re-staging one would just park cruft in incoming/.
        return Ok(Outcome::NonSuccess);
    };
    let success = *success;

    if dry_run {
        tracing::debug!(job_id = %job_id, "requeue-eval: would re-stage");
        return Ok(Outcome::Restaged);
    }

    // Brand-new identity: fresh job_id, now timestamp. Original rows + archive untouched.
    let new_job_id = JobId::from_uuid(uuid::Uuid::now_v7());
    let resubmission = SubmissionInput::Success(Box::new(success.wire)).into_submission(
        success.client_id,
        new_job_id.clone(),
        chrono::Utc::now(),
        benchmark_type,
    );
    let body = serde_json::to_value(&resubmission)?;
    stores
        .submissions
        .write_incoming(&new_job_id, &body)
        .await?;
    tracing::debug!(
        original_job_id = %job_id,
        new_job_id = %new_job_id,
        "requeue-eval: re-staged into incoming under fresh job_id"
    );
    Ok(Outcome::Restaged)
}

/// Build the "not in catalog" error, with a hint when the given string is
/// actually an eval id shared by configured benchmark ids — the common
/// mix-up of passing `ifbench` instead of `eval_ifbench_original`.
fn not_in_catalog_error(
    catalog: &HashMap<BenchmarkId, Benchmark>,
    given: &BenchmarkId,
) -> anyhow::Error {
    let given = given.to_string();
    let mut benchmark_ids: Vec<String> = catalog
        .iter()
        .filter_map(|(id, b)| match &b.def {
            BenchmarkDef::Eval {
                parameter_eval_id, ..
            } if parameter_eval_id == &given => Some(id.to_string()),
            _ => None,
        })
        .collect();
    if benchmark_ids.is_empty() {
        return anyhow::anyhow!("benchmark {given} is not in the catalog");
    }
    benchmark_ids.sort();
    anyhow::anyhow!(
        "{given} is an eval id, not a benchmark id — did you mean one of: {}?",
        benchmark_ids.join(", ")
    )
}

pub async fn run(
    stores: &Stores,
    benchmark_id: &BenchmarkId,
    filters: &Filters,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mode = if dry_run { "dry-run" } else { "live" };
    println!("requeue-eval: starting ({mode}) — benchmark_id={benchmark_id}");
    tracing::info!(
        mode,
        %benchmark_id,
        submitted_after = ?filters.submitted_after,
        submitted_before = ?filters.submitted_before,
        score_runtime_version = ?filters.score_runtime_version,
        "requeue-eval: starting"
    );

    // Validate the benchmark against the configured catalog: it must exist
    // and be an eval. Bail otherwise — the input is a single benchmark, so a
    // bad one is an error, not a silent no-op.
    let catalog = stores.catalog.load_catalog().await?;
    let benchmark = catalog
        .get(benchmark_id)
        .ok_or_else(|| not_in_catalog_error(&catalog, benchmark_id))?;
    let benchmark_type = benchmark.benchmark_type();
    if benchmark_type != BenchmarkType::Eval {
        anyhow::bail!(
            "benchmark {benchmark_id} is not an eval (it is {})",
            benchmark_type.as_ref()
        );
    }

    // Pass 1: collect this benchmark's jobs from the warehouse metrics. The
    // callback never returns `true`, so no Parquet file is rewritten.
    let mut job_ids: HashSet<JobId> = HashSet::new();
    {
        let mut collect = |row: &mut MetricRow| -> bool {
            if &row.benchmark_id == benchmark_id
                && filters.matches(row)
                && let Some(job_id) = job_id_from_result_id(&row.result_id)
            {
                job_ids.insert(job_id);
            }
            false
        };
        stores.warehouse.for_each_metric_row(&mut collect).await?;
    }

    let found = job_ids.len();
    let qualifier = if filters.is_active() { " matching" } else { "" };
    println!("requeue-eval: {found}{qualifier} {benchmark_id} job(s) in the warehouse");
    if found == 0 {
        println!("requeue-eval: nothing to do.");
        return Ok(());
    }

    // Pass 2: reconstruct each job's submission and re-stage it.
    let outcomes: Vec<Outcome> = stream::iter(job_ids.iter().enumerate())
        .then(|(i, job_id)| async move {
            let n = i + 1;
            if n.is_multiple_of(HEARTBEAT_INTERVAL) {
                println!("requeue-eval: processed {n}/{found}");
            }
            restage_one(stores, benchmark_type, job_id, dry_run).await
        })
        .try_collect()
        .await?;

    let tally = |o: Outcome| outcomes.iter().filter(|x| **x == o).count();
    let restaged = tally(Outcome::Restaged);
    let missing = tally(Outcome::MissingBody);
    let unparseable = tally(Outcome::Unparseable);
    let non_success = tally(Outcome::NonSuccess);

    let verb = if dry_run {
        "would re-stage"
    } else {
        "re-staged"
    };
    println!(
        "requeue-eval: done — {verb} {restaged} submission(s); skipped {missing} missing body, \
         {unparseable} unparseable, {non_success} non-success over {found} {benchmark_id} job(s)"
    );
    tracing::info!(
        restaged,
        missing,
        unparseable,
        non_success,
        found,
        dry_run,
        "requeue-eval: done"
    );
    if !dry_run && restaged > 0 {
        println!(
            "requeue-eval: run `pipette-mgmt process-submissions` then `pipette-mgmt score-eval` to re-score the {restaged} re-staged submission(s)."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, StorageConfig};
    use crate::stores::{Stores, build_local_fs_stores};
    use crate::types::ClientId;
    use crate::warehouse::DeviceFormFactor;
    use std::path::Path;

    fn local_config(data_dir: &Path) -> Config {
        Config {
            evals_server_url: "http://unused".to_string(),
            storage: StorageConfig::local_fs(data_dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(data_dir.to_path_buf()),
            ..Config::default()
        }
    }

    fn write_benchmark(dir: &Path, benchmark_id: &str, toml: &str) {
        let bdir = dir.join("benchmarks");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(bdir.join(format!("{benchmark_id}.toml")), toml).unwrap();
    }

    fn eval_toml(eval_id: &str, dataset: &str) -> String {
        format!(
            "benchmark_type = \"eval\"\n\
             parameter_eval_id = \"{eval_id}\"\n\
             parameter_dataset_name = \"{dataset}\"\n\
             parameter_max_tokens = 8192\n"
        )
    }

    const THROUGHPUT_TOML: &str = "benchmark_type = \"decode_throughput\"\n\
         parameter_prefill_tokens = 512\n\
         parameter_decode_tokens = 100\n";

    /// Build local_fs stores with the standard test catalog written to disk
    /// (`run` loads the catalog, so it must exist).
    fn stores_with_catalog(dir: &Path) -> Stores {
        write_benchmark(
            dir,
            "eval_ifbench_original",
            &eval_toml("ifbench", "original"),
        );
        write_benchmark(
            dir,
            "eval_ifbench_2026.04.1",
            &eval_toml("ifbench", "2026.04.1"),
        );
        write_benchmark(
            dir,
            "eval_mmlu_pro_default",
            &eval_toml("mmlu_pro", "default"),
        );
        write_benchmark(dir, "decode_throughput_512", THROUGHPUT_TOML);
        build_local_fs_stores(&local_config(dir)).unwrap()
    }

    /// A metric row for `job_id` (one sample, `result_id = {job_id}_0`).
    /// Selection keys on `benchmark_id`, so `parameter_eval_id` is left
    /// unset — it is not read by `run`.
    fn metric_row(job_id: &str, benchmark_id: &str, benchmark_type: BenchmarkType) -> MetricRow {
        MetricRow {
            result_id: format!("{job_id}_0"),
            benchmark_id: BenchmarkId::try_new(benchmark_id).unwrap(),
            benchmark_type,
            metric: "strict_prompt_accuracy".to_string(),
            client_id: ClientId::try_new("ev1_test").unwrap(),
            device_form_factor: DeviceFormFactor::Laptop,
            device_os_name: "macOS".to_string(),
            device_os_version: "26".to_string(),
            value: 0.5,
            unit: "accuracy".to_string(),
            submitted_at: 1_000_000,
            scored_at: 2_000_000,
            ..Default::default()
        }
    }

    /// A full, valid stored success submission. Carries the server-injected
    /// fields plus a couple of extra keys to prove they're dropped on
    /// reconstruction.
    fn body(job_id: &str, benchmark_id: &str) -> serde_json::Value {
        serde_json::json!({
            "message_type": "success",
            "benchmark_type": "eval",
            "benchmark_id": benchmark_id,
            "client_id": "ev1_test",
            "job_id": job_id,
            "submitted_at": "2026-05-29T14:49:43Z",
            "device_name": "MacBook Pro",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "26",
            "device_chip_model": "Apple M5",
            "device_ram_bytes": 51_539_607_552_i64,
            "model_name": "m",
            "model_quant": "Q5_K_M",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "completions": [{"id": "s1", "completion": "@@@@"}],
            // Stray keys a verbatim copy would carry through; reconstruction
            // through the typed model drops them.
            "scored_at": "2026-05-29T15:00:00Z",
            "_extra": "should be dropped",
        })
    }

    async fn seed(stores: &Stores, job_id: &str, benchmark_id: &str, bt: BenchmarkType) {
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new(benchmark_id).unwrap(),
                &ClientId::try_new("ev1_test").unwrap(),
                "2026-05",
                &[metric_row(job_id, benchmark_id, bt)],
            )
            .await
            .unwrap();
        stores
            .submissions
            .write_processed(&JobId::new_unchecked(job_id), &body(job_id, benchmark_id))
            .await
            .unwrap();
    }

    /// Like `seed`, but overrides the warehouse row's `client_id`,
    /// `submitted_at` (micros), and `score_runtime_version` so filter
    /// behavior can be exercised. The distinct `client_id` is preserved
    /// through re-staging, letting a test assert *which* job survived a
    /// filter, not just how many.
    async fn seed_with(
        stores: &Stores,
        job_id: &str,
        benchmark_id: &str,
        client_id: &str,
        submitted_at: i64,
        score_runtime_version: Option<&str>,
    ) {
        let client = ClientId::try_new(client_id).unwrap();
        let mut row = metric_row(job_id, benchmark_id, BenchmarkType::Eval);
        row.client_id = client.clone();
        row.submitted_at = submitted_at;
        row.score_runtime_version = score_runtime_version.map(str::to_string);
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new(benchmark_id).unwrap(),
                &client,
                "2026-05",
                &[row],
            )
            .await
            .unwrap();
        let mut processed = body(job_id, benchmark_id);
        processed["client_id"] = serde_json::json!(client_id);
        stores
            .submissions
            .write_processed(&JobId::new_unchecked(job_id), &processed)
            .await
            .unwrap();
    }

    fn bid(s: &str) -> BenchmarkId {
        BenchmarkId::try_new(s).unwrap()
    }

    async fn incoming_ids(stores: &Stores) -> Vec<String> {
        let mut ids: Vec<String> = stores
            .submissions
            .list_incoming(crate::TEST_LIST_LIMIT)
            .await
            .unwrap()
            .iter()
            .map(|j| j.to_string())
            .collect();
        ids.sort();
        ids
    }

    /// The `client_id` of every re-staged body in `incoming/`, sorted.
    /// `client_id` survives re-staging, so it identifies which source jobs
    /// were re-staged (the fresh `job_id`s are random and carry no link).
    async fn incoming_client_ids(stores: &Stores) -> Vec<String> {
        let ids = stores
            .submissions
            .list_incoming(crate::TEST_LIST_LIMIT)
            .await
            .unwrap();
        let mut clients: Vec<String> =
            futures::future::join_all(ids.iter().map(|jid| async move {
                let record = stores
                    .submissions
                    .get_submission(jid)
                    .await
                    .unwrap()
                    .unwrap();
                record.body.as_object().unwrap()["client_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            }))
            .await;
        clients.sort();
        clients
    }

    #[tokio::test]
    async fn re_stages_only_the_named_benchmark() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path());
        seed(
            &stores,
            "job-ifb",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await;
        // Same eval, different dataset/benchmark — must NOT be re-staged.
        seed(
            &stores,
            "job-ifb2",
            "eval_ifbench_2026.04.1",
            BenchmarkType::Eval,
        )
        .await;
        // Unrelated eval — must NOT be re-staged.
        seed(
            &stores,
            "job-mmlu",
            "eval_mmlu_pro_default",
            BenchmarkType::Eval,
        )
        .await;

        run(
            &stores,
            &bid("eval_ifbench_original"),
            &Filters::default(),
            false,
        )
        .await?;

        // Exactly one body re-staged, under a fresh (non-original) job_id.
        let staged = incoming_ids(&stores).await;
        assert_eq!(staged.len(), 1, "expected one re-staged body");
        assert_ne!(staged[0], "job-ifb", "re-staged job_id must be fresh");

        // The original processed body is left in place (old data kept).
        let original = stores
            .submissions
            .get_submission(&JobId::new_unchecked("job-ifb"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("original processed body missing"))?;
        assert_eq!(original.state, crate::stores::JobState::Processed);

        // The re-staged body is the canonical submission, written under the
        // fresh job_id; original `client_id` preserved, stray keys dropped
        // by the typed round-trip.
        let new_id = JobId::new_unchecked(staged[0].clone());
        let restaged = stores
            .submissions
            .get_submission(&new_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("re-staged body missing"))?;
        assert_eq!(restaged.state, crate::stores::JobState::Incoming);
        let obj = restaged
            .body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("body not a JSON object"))?;
        assert_eq!(obj["job_id"], staged[0]);
        assert_ne!(obj["job_id"], "job-ifb");
        assert_eq!(obj["client_id"], "ev1_test");
        assert_eq!(obj["message_type"], "success");
        assert!(!obj.contains_key("_extra"), "stray key should be dropped");
        assert!(
            !obj.contains_key("scored_at"),
            "scorer-only key should be dropped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn errors_when_benchmark_missing_or_not_eval() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path());

        let missing = run(
            &stores,
            &bid("eval_ifbench_removed"),
            &Filters::default(),
            false,
        )
        .await
        .unwrap_err();
        assert!(missing.to_string().contains("not in the catalog"));

        let not_eval = run(
            &stores,
            &bid("decode_throughput_512"),
            &Filters::default(),
            false,
        )
        .await
        .unwrap_err();
        assert!(not_eval.to_string().contains("not an eval"));

        assert!(incoming_ids(&stores).await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bare_eval_id_errors_with_benchmark_id_hint() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path());

        // `ifbench` is an eval id, not a benchmark id — the error names the
        // benchmark ids that share it.
        let err = run(&stores, &bid("ifbench"), &Filters::default(), false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("eval id"), "{msg}");
        assert!(msg.contains("eval_ifbench_original"), "{msg}");
        assert!(msg.contains("eval_ifbench_2026.04.1"), "{msg}");
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_re_stages_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path());
        seed(
            &stores,
            "job-ifb",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await;

        run(
            &stores,
            &bid("eval_ifbench_original"),
            &Filters::default(),
            true,
        )
        .await?;

        assert!(incoming_ids(&stores).await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_processed_body_is_skipped_not_fatal() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path());
        // Warehouse row exists, but no processed body for the job.
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new("eval_ifbench_original")?,
                &ClientId::try_new("ev1_test")?,
                "2026-05",
                &[metric_row(
                    "job-orphan",
                    "eval_ifbench_original",
                    BenchmarkType::Eval,
                )],
            )
            .await?;

        run(
            &stores,
            &bid("eval_ifbench_original"),
            &Filters::default(),
            false,
        )
        .await?;

        assert!(incoming_ids(&stores).await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn dedupes_multi_sample_jobs_to_one_re_stage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path());
        // Two metric rows (two samples) for the same job.
        let mut r0 = metric_row("job-ifb", "eval_ifbench_original", BenchmarkType::Eval);
        let mut r1 = r0.clone();
        r0.result_id = "job-ifb_0".to_string();
        r1.result_id = "job-ifb_1".to_string();
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new("eval_ifbench_original")?,
                &ClientId::try_new("ev1_test")?,
                "2026-05",
                &[r0, r1],
            )
            .await?;
        stores
            .submissions
            .write_processed(
                &JobId::new_unchecked("job-ifb"),
                &body("job-ifb", "eval_ifbench_original"),
            )
            .await?;

        run(
            &stores,
            &bid("eval_ifbench_original"),
            &Filters::default(),
            false,
        )
        .await?;

        // Two samples for the same source job_id still produce exactly one
        // re-staged body (deduplicated upstream), under a fresh job_id.
        let staged = incoming_ids(&stores).await;
        assert_eq!(staged.len(), 1);
        assert_ne!(staged[0], "job-ifb");
        Ok(())
    }

    #[tokio::test]
    async fn filters_narrow_the_selected_jobs() -> anyhow::Result<()> {
        struct Case {
            name: &'static str,
            filters: Filters,
            /// `client_id`s of the jobs that should be re-staged, sorted.
            expected: &'static [&'static str],
        }
        // Two original jobs for the same benchmark, differing in client_id,
        // submitted_at, and score_runtime_version, so each filter can be
        // shown to keep exactly the right job — not just the right count.
        let cases = [
            Case {
                name: "no filter selects both",
                filters: Filters::default(),
                expected: &["ev1_new", "ev1_old"],
            },
            Case {
                name: "submitted_before keeps only the older job",
                filters: Filters {
                    submitted_before: Some(3_000_000),
                    ..Default::default()
                },
                expected: &["ev1_old"],
            },
            Case {
                name: "submitted_after keeps only the newer job",
                filters: Filters {
                    submitted_after: Some(3_000_000),
                    ..Default::default()
                },
                expected: &["ev1_new"],
            },
            Case {
                name: "score_runtime_version matches exactly",
                filters: Filters {
                    score_runtime_version: Some("v2".to_string()),
                    ..Default::default()
                },
                expected: &["ev1_new"],
            },
            Case {
                name: "non-matching version selects none",
                filters: Filters {
                    score_runtime_version: Some("nope".to_string()),
                    ..Default::default()
                },
                expected: &[],
            },
        ];

        for case in cases {
            let dir = tempfile::tempdir()?;
            let stores = stores_with_catalog(dir.path());
            seed_with(
                &stores,
                "job-old",
                "eval_ifbench_original",
                "ev1_old",
                1_000_000,
                Some("v1"),
            )
            .await;
            seed_with(
                &stores,
                "job-new",
                "eval_ifbench_original",
                "ev1_new",
                5_000_000,
                Some("v2"),
            )
            .await;

            run(&stores, &bid("eval_ifbench_original"), &case.filters, false).await?;

            let got = incoming_client_ids(&stores).await;
            let got: Vec<&str> = got.iter().map(String::as_str).collect();
            assert_eq!(got.as_slice(), case.expected, "{}", case.name);
        }
        Ok(())
    }
}
