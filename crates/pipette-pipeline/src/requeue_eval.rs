//! `requeue-eval`: re-stage already-scored eval and vlm_eval submissions into
//! `incoming/` under fresh job ids, since a processed body is never re-scored.
//! Originals stay; it writes `incoming/`, so the CLI holds the storage mutate lock.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, TryStreamExt, stream};

use pipette_core::benchmark::Benchmark;
use pipette_core::warehouse::MetricRow;
use pipette_plan_types::{
    AsrEvalBenchmark, BenchmarkDefinition, BenchmarkId, BenchmarkType, EvalBenchmark, JobId,
    SubmissionInput, VlmEvalBenchmark,
};
use pipette_store::Stores;

use crate::submission::{RequestedFlags, Submission, SubmissionInputExt, parse_stored_submission};

const HEARTBEAT_INTERVAL: usize = 1000;

/// Constraints narrowing which warehouse jobs are re-staged, AND-combined.
/// Re-staging is not idempotent, so pin `submitted_before` to before the first
/// re-stage: every copy, submitted after it, falls outside.
#[derive(Clone, Default)]
pub struct Filters {
    /// Inclusive lower bound on the row's `submitted_at` (micros since epoch).
    pub submitted_after: Option<i64>,
    /// Inclusive upper bound on the row's `submitted_at` (micros since epoch).
    pub submitted_before: Option<i64>,
    /// Exact match on the row's `score_runtime_version`, as the scoring service
    /// returned it.
    pub score_runtime_version: Option<String>,
}

impl Filters {
    /// Whether a warehouse row clears every active constraint. A failure row never
    /// does: it has no completions to re-score.
    fn matches(&self, row: &MetricRow) -> bool {
        row.failed != Some(true)
            && self.submitted_after.is_none_or(|t| row.submitted_at >= t)
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
    Unparseable,
    /// Body is a `message_type: "failure"` submission (not scorable).
    NonSuccess,
}

/// The `job_id` of a `{job_id}_{i}` result id; a UUID has no underscore. A
/// result id without one is the whole id.
fn job_id_from_result_id(result_id: &str) -> Option<JobId> {
    let jid = result_id
        .rsplit_once('_')
        .map(|(j, _)| j)
        .unwrap_or(result_id);
    // `try_new` rejects the empty string and any unsafe char, so a malformed
    // result id yields `None` (skipped) rather than a dangerous JobId.
    JobId::try_new(jid)
        .inspect_err(|e| {
            tracing::warn!(result_id, error = %e, "requeue-eval: result id names no job, skipping");
        })
        .ok()
}

/// Reconstruct one job's submission and, unless `dry_run`, write it to
/// `incoming/` under a fresh `job_id` and `submitted_at = now`.
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

    // Tolerates legacy bodies without `message_type`.
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

    let run_submitted_at =
        crate::recalculation::run_submitted_at(stores.submissions.as_ref(), &success).await?;
    // Brand-new identity: fresh job_id, now timestamp. Original rows + archive untouched.
    let new_job_id = JobId::from_uuid(uuid::Uuid::now_v7());
    let mut resubmission = SubmissionInput::Success(Box::new(success.wire)).into_submission(
        success.client_id,
        new_job_id.clone(),
        chrono::Utc::now(),
        benchmark_type,
    );
    // The copy answers the job the original did, so it keeps the flags that
    // job asked for; `/score` reads `enable_thinking` from them.
    resubmission.set_requested_flags(RequestedFlags {
        model: success.requested_model_flags,
        runtime: success.requested_runtime_flags,
        benchmark: success.requested_benchmark_flags,
    });
    if let Submission::Success(copy) = &mut resubmission {
        copy.rescored_from = Some(job_id.clone());
        copy.run_submitted_at = run_submitted_at;
    }
    let body = serde_json::to_value(&resubmission)?;
    stores
        .submissions
        .write_incoming(&new_job_id, &body)
        .await?;
    tracing::debug!(
        original_job_id = %job_id,
        new_job_id = %new_job_id,
        run_submitted_at = ?run_submitted_at,
        "requeue-eval: re-staged into incoming under fresh job_id"
    );
    Ok(Outcome::Restaged)
}

/// The "not in catalog" error, hinting at the benchmark ids when given an
/// eval id (`ifbench` for `eval_ifbench_original`).
fn not_in_catalog_error(
    catalog: &HashMap<BenchmarkId, Benchmark>,
    given: &BenchmarkId,
) -> anyhow::Error {
    let given = given.to_string();
    let mut benchmark_ids: Vec<String> = catalog
        .iter()
        .filter_map(|(id, b)| match &b.def {
            BenchmarkDefinition::Eval(EvalBenchmark {
                parameter_eval_id, ..
            }) if parameter_eval_id.as_str() == given => Some(id.to_string()),
            BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
                parameter_lmms_task,
                ..
            }) if parameter_lmms_task.as_str() == given => Some(id.to_string()),
            BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
                parameter_asr_task, ..
            }) if parameter_asr_task.as_str() == given => Some(id.to_string()),
            BenchmarkDefinition::Eval(_)
            | BenchmarkDefinition::VlmEval(_)
            | BenchmarkDefinition::AsrEval(_)
            | BenchmarkDefinition::PrefillThroughput(_)
            | BenchmarkDefinition::DecodeThroughput(_)
            | BenchmarkDefinition::EndToEndLatency(_)
            | BenchmarkDefinition::MaxMemoryUsage(_)
            | BenchmarkDefinition::VlThroughput(_)
            | BenchmarkDefinition::VlMaxMemory(_)
            | BenchmarkDefinition::AsrThroughput(_)
            | BenchmarkDefinition::AsrMaxMemory(_)
            | BenchmarkDefinition::VlSpecDecode(_) => None,
        })
        .collect();
    if benchmark_ids.is_empty() {
        return anyhow::anyhow!("benchmark {given} is not in the catalog");
    }
    benchmark_ids.sort();
    anyhow::anyhow!(
        "{given} is an eval id, not a benchmark id. Did you mean one of: {}?",
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
    println!("requeue-eval: starting ({mode}) for benchmark_id={benchmark_id}");
    tracing::info!(
        mode,
        %benchmark_id,
        submitted_after = ?filters.submitted_after,
        submitted_before = ?filters.submitted_before,
        score_runtime_version = ?filters.score_runtime_version,
        "requeue-eval: starting"
    );

    // A single benchmark is named, so an unknown one, or one the scoring
    // service doesn't score (eval, vlm_eval, asr_eval), is an error.
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let benchmark = catalog
        .get(benchmark_id)
        .ok_or_else(|| not_in_catalog_error(&catalog, benchmark_id))?;
    let benchmark_type = benchmark.benchmark_type();
    if !benchmark_type.is_scored_via_server() {
        anyhow::bail!(
            "benchmark {benchmark_id} is not an eval, vlm_eval or asr_eval (it is {benchmark_type})"
        );
    }

    // Pass 1: this benchmark's jobs. The callback never returns `true`, so no
    // Parquet file is rewritten.
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
        "requeue-eval: done. {verb} {restaged} submission(s); skipped {missing} missing body, \
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
    use std::path::Path;

    use anyhow::Context;

    use pipette_core::config::{Config, StorageConfig};
    use pipette_plan_types::{ClientId, DeviceFormFactor};
    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    fn local_config(data_dir: &Path) -> Config {
        Config {
            evals_server_url: "http://unused".to_string(),
            storage: StorageConfig::local_fs(data_dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(data_dir.to_path_buf()),
            ..Config::default()
        }
    }

    fn write_benchmark(dir: &Path, benchmark_id: &str, toml: &str) -> anyhow::Result<()> {
        let bdir = dir.join("benchmarks");
        std::fs::create_dir_all(&bdir)?;
        std::fs::write(bdir.join(format!("{benchmark_id}.toml")), toml)?;
        Ok(())
    }

    fn eval_toml(eval_id: &str, dataset: &str) -> String {
        format!(
            "benchmark_type = \"eval\"\n\
             parameter_eval_id = \"{eval_id}\"\n\
             parameter_dataset_name = \"{dataset}\"\n\
             parameter_max_tokens = 8192\n"
        )
    }

    fn vlm_eval_toml() -> String {
        format!(
            "benchmark_type = \"vlm_eval\"\n\
             parameter_lmms_task = \"mmmu_val\"\n\
             parameter_dataset_version = \"v1\"\n\
             parameter_sample_count = 200\n\
             parameter_max_tokens = 32\n{}",
            pipette_core::dataset::vlm_eval_catalog_tables("mmmu_val", "v1", None)
        )
    }

    fn asr_eval_toml() -> String {
        format!(
            "benchmark_type = \"asr_eval\"\n\
             parameter_asr_task = \"asr_fleurs_nl_nl\"\n\
             parameter_dataset_version = \"2026.10.1\"\n\
             parameter_sample_count = 364\n\
             parameter_prompt_profile = \"voxtral\"\n\
             parameter_max_tokens = 500\n{}",
            pipette_core::dataset::asr_eval_catalog_tables(
                "asr_fleurs_nl_nl",
                "2026.10.1",
                "voxtral"
            )
        )
    }

    const THROUGHPUT_TOML: &str = "benchmark_type = \"decode_throughput\"\n\
         parameter_prefill_tokens = 512\n\
         parameter_decode_tokens = 100\n";

    /// Local stores with the test catalog on disk, which `run` loads.
    fn stores_with_catalog(dir: &Path) -> anyhow::Result<Stores> {
        write_benchmark(
            dir,
            "eval_ifbench_original",
            &eval_toml("ifbench", "original"),
        )?;
        write_benchmark(
            dir,
            "eval_ifbench_2026.04.1",
            &eval_toml("ifbench", "2026.04.1"),
        )?;
        write_benchmark(
            dir,
            "eval_mmlu_pro_default",
            &eval_toml("mmlu_pro", "default"),
        )?;
        write_benchmark(dir, "decode_throughput_512", THROUGHPUT_TOML)?;
        write_benchmark(dir, "vlm_eval_mmmu_val", &vlm_eval_toml())?;
        write_benchmark(dir, "asr_eval_fleurs_nl_nl_voxtral", &asr_eval_toml())?;
        build_local_fs_stores(&local_config(dir))
    }

    /// A metric row for `job_id`; selection keys on `benchmark_id` alone.
    fn metric_row(
        job_id: &str,
        benchmark_id: &str,
        benchmark_type: BenchmarkType,
    ) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: format!("{job_id}_0"),
            benchmark_id: BenchmarkId::try_new(benchmark_id)?,
            benchmark_type,
            metric: "strict_prompt_accuracy".to_string(),
            client_id: ClientId::try_new("ev1_test")?,
            device_form_factor: Some(DeviceFormFactor::Laptop),
            device_os_name: Some("macOS".to_string()),
            device_os_version: Some("26".to_string()),
            value: Some(0.5),
            unit: "accuracy".to_string(),
            submitted_at: 1_000_000,
            scored_at: 2_000_000,
            ..Default::default()
        })
    }

    /// A valid stored success submission, with extra keys reconstruction drops.
    fn body(job_id: &str, benchmark_id: &str) -> serde_json::Value {
        body_of(job_id, benchmark_id, BenchmarkType::Eval)
    }

    fn body_of(job_id: &str, benchmark_id: &str, bt: BenchmarkType) -> serde_json::Value {
        serde_json::json!({
            "message_type": "success",
            "benchmark_type": bt.to_string(),
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

    async fn seed(
        stores: &Stores,
        job_id: &str,
        benchmark_id: &str,
        bt: BenchmarkType,
    ) -> anyhow::Result<()> {
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new(benchmark_id)?,
                &ClientId::try_new("ev1_test")?,
                "2026-05",
                &[metric_row(job_id, benchmark_id, bt)?],
            )
            .await?;
        stores
            .submissions
            .write_processed(
                &JobId::new_unchecked(job_id),
                &body_of(job_id, benchmark_id, bt),
            )
            .await?;
        Ok(())
    }

    /// `seed` with the row's `client_id`, `submitted_at` (micros) and
    /// `score_runtime_version` overridden; the `client_id` names the survivor.
    async fn seed_with(
        stores: &Stores,
        job_id: &str,
        benchmark_id: &str,
        client_id: &str,
        submitted_at: i64,
        score_runtime_version: Option<&str>,
    ) -> anyhow::Result<()> {
        let client = ClientId::try_new(client_id)?;
        let mut row = metric_row(job_id, benchmark_id, BenchmarkType::Eval)?;
        row.client_id = client.clone();
        row.submitted_at = submitted_at;
        row.score_runtime_version = score_runtime_version.map(str::to_string);
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new(benchmark_id)?,
                &client,
                "2026-05",
                &[row],
            )
            .await?;
        let mut processed = body(job_id, benchmark_id);
        processed["client_id"] = serde_json::json!(client_id);
        stores
            .submissions
            .write_processed(&JobId::new_unchecked(job_id), &processed)
            .await?;
        Ok(())
    }

    fn bid(s: &str) -> anyhow::Result<BenchmarkId> {
        Ok(BenchmarkId::try_new(s)?)
    }

    async fn incoming_ids(stores: &Stores) -> anyhow::Result<Vec<String>> {
        let mut ids: Vec<String> = stores
            .submissions
            .list_incoming(pipette_store_testkit::TEST_LIST_LIMIT)
            .await?
            .iter()
            .map(|j| j.to_string())
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// The sorted `client_id` of every re-staged body: it survives re-staging, so
    /// it names the source jobs.
    async fn incoming_client_ids(stores: &Stores) -> anyhow::Result<Vec<String>> {
        let ids = stores
            .submissions
            .list_incoming(pipette_store_testkit::TEST_LIST_LIMIT)
            .await?;
        let mut clients: Vec<String> =
            futures::future::try_join_all(ids.iter().map(|jid| async move {
                let record = stores
                    .submissions
                    .get_submission(jid)
                    .await?
                    .context("a listed submission must still be readable")?;
                let client_id = record
                    .body
                    .as_object()
                    .context("a stored submission body is a JSON object")?["client_id"]
                    .as_str()
                    .context("a stored submission carries a string client_id")?
                    .to_string();
                anyhow::Ok(client_id)
            }))
            .await?;
        clients.sort();
        Ok(clients)
    }

    #[tokio::test]
    async fn re_stages_only_the_named_benchmark() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        seed(
            &stores,
            "job-ifb",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await?;
        // Same eval, different dataset/benchmark — must NOT be re-staged.
        seed(
            &stores,
            "job-ifb2",
            "eval_ifbench_2026.04.1",
            BenchmarkType::Eval,
        )
        .await?;
        // Unrelated eval — must NOT be re-staged.
        seed(
            &stores,
            "job-mmlu",
            "eval_mmlu_pro_default",
            BenchmarkType::Eval,
        )
        .await?;

        run(
            &stores,
            &bid("eval_ifbench_original")?,
            &Filters::default(),
            false,
        )
        .await?;

        // Exactly one body re-staged, under a fresh (non-original) job_id.
        let staged = incoming_ids(&stores).await?;
        assert_eq!(staged.len(), 1, "expected one re-staged body");
        assert_ne!(staged[0], "job-ifb", "re-staged job_id must be fresh");

        // The original processed body is left in place (old data kept).
        let original = stores
            .submissions
            .get_submission(&JobId::new_unchecked("job-ifb"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("original processed body missing"))?;
        assert_eq!(original.state, pipette_store::JobState::Processed);

        // Original `client_id` kept, stray keys dropped by the typed round-trip.
        let new_id = JobId::new_unchecked(staged[0].clone());
        let restaged = stores
            .submissions
            .get_submission(&new_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("re-staged body missing"))?;
        assert_eq!(restaged.state, pipette_store::JobState::Incoming);
        let obj = restaged
            .body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("body not a JSON object"))?;
        assert_eq!(obj["job_id"], staged[0]);
        assert_ne!(obj["job_id"], "job-ifb");
        assert_eq!(obj["client_id"], "ev1_test");
        assert_eq!(obj["message_type"], "success");
        assert_eq!(obj["rescored_from"], "job-ifb");
        assert_eq!(obj["run_submitted_at"], "2026-05-29T14:49:43Z");
        assert!(!obj.contains_key("_extra"), "stray key should be dropped");
        assert!(
            !obj.contains_key("scored_at"),
            "scorer-only key should be dropped"
        );
        Ok(())
    }

    /// A `vlm_eval` submission is re-staged the same way an `eval` one is:
    /// fresh job_id in `incoming/`, original processed body untouched, and
    /// the reconstructed body keeps `benchmark_type: "vlm_eval"`.
    #[tokio::test]
    async fn re_stages_vlm_eval_submissions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        seed(
            &stores,
            "job-vlm",
            "vlm_eval_mmmu_val",
            BenchmarkType::VlmEval,
        )
        .await?;
        // Unrelated eval — must NOT be re-staged.
        seed(
            &stores,
            "job-ifb",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await?;

        run(
            &stores,
            &bid("vlm_eval_mmmu_val")?,
            &Filters::default(),
            false,
        )
        .await?;

        // Exactly one body re-staged, under a fresh (non-original) job_id.
        let staged = incoming_ids(&stores).await?;
        assert_eq!(staged.len(), 1, "expected one re-staged body");
        assert_ne!(staged[0], "job-vlm", "re-staged job_id must be fresh");

        // The original processed body is left in place.
        let original = stores
            .submissions
            .get_submission(&JobId::new_unchecked("job-vlm"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("original processed body missing"))?;
        assert_eq!(original.state, pipette_store::JobState::Processed);

        // The re-staged body is the canonical submission and stays a
        // vlm_eval (the catalog-resolved benchmark_type is re-attached).
        let new_id = JobId::new_unchecked(staged[0].clone());
        let restaged = stores
            .submissions
            .get_submission(&new_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("re-staged body missing"))?;
        assert_eq!(restaged.state, pipette_store::JobState::Incoming);
        let obj = restaged
            .body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("body not a JSON object"))?;
        assert_eq!(obj["job_id"], staged[0]);
        assert_eq!(obj["benchmark_type"], "vlm_eval");
        assert_eq!(obj["benchmark_id"], "vlm_eval_mmmu_val");
        assert_eq!(obj["client_id"], "ev1_test");
        assert_eq!(obj["message_type"], "success");
        Ok(())
    }

    /// An `asr_eval` submission is re-staged as a `vlm_eval` one is, and keeps
    /// its `benchmark_type`.
    #[tokio::test]
    async fn re_stages_asr_eval_submissions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        seed(
            &stores,
            "job-asr",
            "asr_eval_fleurs_nl_nl_voxtral",
            BenchmarkType::AsrEval,
        )
        .await?;
        // Unrelated eval — must NOT be re-staged.
        seed(
            &stores,
            "job-vlm",
            "vlm_eval_mmmu_val",
            BenchmarkType::VlmEval,
        )
        .await?;

        run(
            &stores,
            &bid("asr_eval_fleurs_nl_nl_voxtral")?,
            &Filters::default(),
            false,
        )
        .await?;

        let staged = incoming_ids(&stores).await?;
        let [new_id] = staged.as_slice() else {
            anyhow::bail!("not one re-staged body: {staged:?}");
        };
        assert_ne!(new_id, "job-asr", "re-staged job_id must be fresh");
        let original = stores
            .submissions
            .get_submission(&JobId::new_unchecked("job-asr"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("original processed body missing"))?;
        assert_eq!(original.state, pipette_store::JobState::Processed);
        let restaged = stores
            .submissions
            .get_submission(&JobId::new_unchecked(new_id.clone()))
            .await?
            .ok_or_else(|| anyhow::anyhow!("re-staged body missing"))?;
        assert_eq!(restaged.state, pipette_store::JobState::Incoming);
        assert_eq!(restaged.body["benchmark_type"], "asr_eval");
        assert_eq!(
            restaged.body["benchmark_id"],
            "asr_eval_fleurs_nl_nl_voxtral"
        );
        assert_eq!(restaged.body["rescored_from"], "job-asr");
        Ok(())
    }

    /// A bare asr_eval task id names the benchmark ids that run it.
    #[tokio::test]
    async fn bare_asr_task_errors_with_benchmark_id_hint() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        let err = run(
            &stores,
            &bid("asr_fleurs_nl_nl")?,
            &Filters::default(),
            false,
        )
        .await
        .err()
        .context("run must reject this")?;
        let msg = err.to_string();
        assert!(msg.contains("eval id"), "{msg}");
        assert!(msg.contains("asr_eval_fleurs_nl_nl_voxtral"), "{msg}");
        Ok(())
    }

    /// Re-staging a copy names the copy, one level up, not the job a device
    /// ran. Its run time is the device run's, read through a copy that does
    /// not carry one.
    #[tokio::test]
    async fn a_copy_of_a_copy_names_its_parent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        seed(
            &stores,
            "job-copy",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await?;
        let mut copy = body("job-copy", "eval_ifbench_original");
        copy["rescored_from"] = serde_json::json!("job-device");
        copy["submitted_at"] = serde_json::json!("2026-06-10T08:00:00Z");
        stores
            .submissions
            .write_processed(&JobId::new_unchecked("job-copy"), &copy)
            .await?;
        stores
            .submissions
            .write_processed(
                &JobId::new_unchecked("job-device"),
                &body("job-device", "eval_ifbench_original"),
            )
            .await?;

        run(
            &stores,
            &bid("eval_ifbench_original")?,
            &Filters::default(),
            false,
        )
        .await?;

        let staged = incoming_ids(&stores).await?;
        let [new_id] = staged.as_slice() else {
            anyhow::bail!("not one re-staged body: {staged:?}");
        };
        let restaged = stores
            .submissions
            .get_submission(&JobId::new_unchecked(new_id.clone()))
            .await?
            .context("re-staged body missing")?;
        assert_eq!(restaged.body["rescored_from"], "job-copy");
        assert_eq!(restaged.body["run_submitted_at"], "2026-05-29T14:49:43Z");
        Ok(())
    }

    /// Re-scoring sends `/score` the job's `enable_thinking`.
    #[tokio::test]
    async fn a_copy_keeps_the_flags_its_job_asked_for() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        seed(
            &stores,
            "job-ifb",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await?;
        let requested = [
            (
                "requested_model_flags",
                r#"{"benchmark_type":"eval","enable_thinking":true,"model_type":"gguf_text"}"#,
            ),
            (
                "requested_runtime_flags",
                r#"{"runtime_type":"llamacpp_cli_stock_tools","threads":8}"#,
            ),
            (
                "requested_benchmark_flags",
                r#"{"benchmark_type":"eval","http_timeout_seconds":600}"#,
            ),
        ];
        let mut original = body("job-ifb", "eval_ifbench_original");
        for (key, value) in requested {
            original[key] = serde_json::json!(value);
        }
        stores
            .submissions
            .write_processed(&JobId::new_unchecked("job-ifb"), &original)
            .await?;

        run(
            &stores,
            &bid("eval_ifbench_original")?,
            &Filters::default(),
            false,
        )
        .await?;

        let staged = incoming_ids(&stores).await?;
        let [new_id] = staged.as_slice() else {
            anyhow::bail!("not one re-staged body: {staged:?}");
        };
        let restaged = stores
            .submissions
            .get_submission(&JobId::new_unchecked(new_id.clone()))
            .await?
            .context("re-staged body missing")?;
        for (key, value) in requested {
            assert_eq!(restaged.body[key], value, "{key}");
        }
        Ok(())
    }

    /// The host resolves every organization's catalog.
    #[tokio::test]
    async fn re_stages_a_private_eval() -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        let id = "eval_acme_internal_v3";
        stores
            .catalog
            .put_org_benchmark(
                &pipette_plan_types::OrgId::mint(),
                &Benchmark::from_toml(id, &eval_toml("acme_internal", "v3"))?,
            )
            .await?;
        seed(&stores, "job-acme", id, BenchmarkType::Eval).await?;

        run(&stores, &bid(id)?, &Filters::default(), false).await?;
        assert_eq!(incoming_ids(&stores).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn errors_when_benchmark_missing_or_not_eval() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;

        let missing = run(
            &stores,
            &bid("eval_ifbench_removed")?,
            &Filters::default(),
            false,
        )
        .await
        .err()
        .context("run must reject this")?;
        assert!(missing.to_string().contains("not in the catalog"));

        let not_eval = run(
            &stores,
            &bid("decode_throughput_512")?,
            &Filters::default(),
            false,
        )
        .await
        .err()
        .context("run must reject this")?;
        assert!(not_eval.to_string().contains("not an eval"));

        assert!(incoming_ids(&stores).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bare_eval_id_errors_with_benchmark_id_hint() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;

        // `ifbench` is an eval id, not a benchmark id — the error names the
        // benchmark ids that share it.
        let err = run(&stores, &bid("ifbench")?, &Filters::default(), false)
            .await
            .err()
            .context("run must reject this")?;
        let msg = err.to_string();
        assert!(msg.contains("eval id"), "{msg}");
        assert!(msg.contains("eval_ifbench_original"), "{msg}");
        assert!(msg.contains("eval_ifbench_2026.04.1"), "{msg}");
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_re_stages_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        seed(
            &stores,
            "job-ifb",
            "eval_ifbench_original",
            BenchmarkType::Eval,
        )
        .await?;

        run(
            &stores,
            &bid("eval_ifbench_original")?,
            &Filters::default(),
            true,
        )
        .await?;

        assert!(incoming_ids(&stores).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_processed_body_is_skipped_not_fatal() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
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
                )?],
            )
            .await?;

        run(
            &stores,
            &bid("eval_ifbench_original")?,
            &Filters::default(),
            false,
        )
        .await?;

        assert!(incoming_ids(&stores).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn dedupes_multi_sample_jobs_to_one_re_stage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_with_catalog(dir.path())?;
        // Two metric rows (two samples) for the same job.
        let mut r0 = metric_row("job-ifb", "eval_ifbench_original", BenchmarkType::Eval)?;
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
            &bid("eval_ifbench_original")?,
            &Filters::default(),
            false,
        )
        .await?;

        // Two samples of one job still re-stage one body.
        let staged = incoming_ids(&stores).await?;
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
        // Two jobs differing in client_id, submitted_at and score_runtime_version, so
        // each filter is shown to keep the right job, not just the right count.
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
            let stores = stores_with_catalog(dir.path())?;
            seed_with(
                &stores,
                "job-old",
                "eval_ifbench_original",
                "ev1_old",
                1_000_000,
                Some("v1"),
            )
            .await?;
            seed_with(
                &stores,
                "job-new",
                "eval_ifbench_original",
                "ev1_new",
                5_000_000,
                Some("v2"),
            )
            .await?;

            run(
                &stores,
                &bid("eval_ifbench_original")?,
                &case.filters,
                false,
            )
            .await?;

            let got = incoming_client_ids(&stores).await?;
            let got: Vec<&str> = got.iter().map(String::as_str).collect();
            assert_eq!(got.as_slice(), case.expected, "{}", case.name);
        }
        Ok(())
    }
}
