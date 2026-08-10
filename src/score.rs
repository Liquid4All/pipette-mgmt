use std::collections::{HashMap, HashSet};

use chrono::Utc;

use crate::benchmark::{Benchmark, BenchmarkDef};
use crate::config::Config;
use crate::eval_sample_result::EvalSampleResult;
use crate::model_params::ModelCatalog;
use crate::scoring_service::{
    self, SampleCompletion, ScoreRequest, ScoreRequestSample, ScoringError,
};
use crate::stores::{JobState, ScoreQueueStage, Stores, SubmissionRecord};
pub use crate::submission::{FailureSubmission, Submission, SuccessSubmission};
use crate::types::{BenchmarkId, ClientId, JobId};
use crate::warehouse::{self, DeviceFormFactor, MetricRow};

/// Cap on `failed_jobs.len()` over-fetch (see `run_score`). One S3 LIST page
/// is 1000 objects, so capping the over-fetch at one extra page bounds
/// per-iteration LIST cost to at most 2 round-trips for any
/// `chunk_limit ≤ 1000`. Past this many failures, items at the back of the
/// lex prefix may not be reached this invocation; the next cron tick picks
/// them up with a fresh `failed_jobs` set.
const FETCH_OVERHEAD_CAP: usize = 1000;

pub async fn run_process_submissions(config: &Config, stores: Stores) -> anyhow::Result<()> {
    let chunk_limit = config.score_chunk_size;

    let catalog = stores.catalog.load_catalog().await?;
    // Load the model catalog fresh on each invocation. This is a
    // short-lived cron, so an at-startup snapshot is the right
    // staleness window — the next tick picks up any edits to
    // `model_params_mapping.toml`.
    let model_catalog =
        ModelCatalog::load(&config.storage, config.model_params_mapping_path.as_deref()).await?;
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .build()?;

    // Eval submissions are routed to `score-queue/to_do/` inline by
    // `read_and_score` as the chunk loop encounters them (so this fast worker
    // never makes a scoring-service call); `routed` counts those.
    let mut routed = 0usize;

    // Failed keys accumulate across chunks within this single invocation so
    // they are skipped on subsequent `list_incoming` calls. Without this guard,
    // the same failing items would be re-listed and re-attempted indefinitely.
    // Failed items stay in `incoming/` and are retried on the next invocation.
    //
    // Listings over-fetch by `failed_jobs.len()` (capped) so that after
    // filtering we still have up to `chunk_limit` candidate items. List
    // ordering is implementation-defined (S3: lexicographic) and may surface
    // failed items first; without over-fetch an empty filtered chunk could
    // mask genuinely-pending work and the backlog would not drain.
    //
    // Each chunk is committed before the next list: warehouse metrics +
    // eval sample results + `mark_processed`. The commit must run inside
    // the loop because `mark_processed` is what removes successfully-scored
    // items from `incoming/`; without it the next `list_incoming` would
    // re-surface them and the loop wouldn't terminate. `mark_processed`
    // also runs *after* the warehouse write so a crash never leaves an
    // item marked processed without its metrics persisted.
    let mut failed_jobs: HashSet<JobId> = HashSet::new();
    let mut total_scored = 0usize;
    let mut total_ignored_failures = 0usize;
    let mut chunk_idx = 0usize;

    loop {
        let overhead = failed_jobs.len().min(FETCH_OVERHEAD_CAP);
        let fetch_limit = chunk_limit.saturating_add(overhead);
        let listed = stores.submissions.list_incoming(fetch_limit).await?;
        let listed_len = listed.len();
        let chunk: Vec<JobId> = listed
            .into_iter()
            .filter(|j| !failed_jobs.contains(j))
            .take(chunk_limit.get())
            .collect();

        if chunk.is_empty() {
            break;
        }

        chunk_idx += 1;
        let chunk_size = chunk.len();
        println!("Scoring chunk {chunk_idx} ({chunk_size} submission(s), limit {chunk_limit})...");
        tracing::info!(
            chunk = chunk_idx,
            chunk_size,
            chunk_limit = chunk_limit.get(),
            failed_so_far = failed_jobs.len(),
            "scoring chunk"
        );

        let outcome = score_chunk(
            config,
            &catalog,
            &model_catalog,
            &stores,
            &http_client,
            chunk_idx,
            &chunk,
        )
        .await;
        let deferred: HashSet<JobId> = commit_chunk(&stores, &outcome.outcomes)
            .await?
            .into_iter()
            .collect();
        for o in &outcome.outcomes {
            match o {
                // A scored job that couldn't commit is deferred — it stays in
                // `incoming/` and is counted as failed (via `failed_jobs`
                // below), not scored.
                ScoreOutcome::Scored(s) if deferred.contains(&s.job_id) => {}
                ScoreOutcome::Scored(_) => total_scored += 1,
                // Eval routed to `to_do` by the inline guard; already out of
                // `incoming/`, nothing to commit.
                ScoreOutcome::Routed { .. } => routed += 1,
                ScoreOutcome::IgnoredFailure { job_id } => {
                    total_ignored_failures += 1;
                    // Keep them in `incoming/` but skip on subsequent
                    // chunks of this run so we don't re-parse the same
                    // body N times when nothing changes.
                    failed_jobs.insert(job_id.clone());
                }
            }
        }
        failed_jobs.extend(deferred);
        failed_jobs.extend(outcome.newly_failed);

        // Listing was not fully populated, so the store is exhausted (modulo
        // failed items). Skip the extra empty LIST that the next iteration
        // would issue.
        if listed_len < fetch_limit.get() {
            break;
        }
    }

    // Finalize eval jobs the slow `score-eval` worker has staged into
    // `score-queue/to_finalize/` — derive from the stored response (no
    // network), write the warehouse + eval rows, archive, dequeue.
    let finalized = finalize_scored_evals(
        config,
        &catalog,
        &model_catalog,
        &http_client,
        &stores,
        chunk_limit,
    )
    .await?;

    let total_failed = failed_jobs.len().saturating_sub(total_ignored_failures);
    let total_seen = total_scored + total_ignored_failures + total_failed;
    println!(
        "Done. {routed} eval(s) routed to scoring; {total_seen} non-eval submission(s) across \
         {chunk_idx} chunk(s): {total_scored} scored, {total_ignored_failures} ignored failure(s), \
         {total_failed} failed; {finalized} eval(s) finalized."
    );

    if total_failed > 0 {
        tracing::warn!(
            failed = total_failed,
            "some submissions failed to score, will retry on next run"
        );
    }
    Ok(())
}

/// Whether a stored submission body is an eval benchmark — the only type that
/// needs the slow scoring-service call. Non-success / unknown bodies are not.
fn submission_is_eval(catalog: &HashMap<BenchmarkId, Benchmark>, body: &serde_json::Value) -> bool {
    match crate::submission::parse_stored_submission(body) {
        Ok(Submission::Success(s)) => catalog
            .get(&s.wire.benchmark_id)
            .is_some_and(|b| matches!(b.def, BenchmarkDef::Eval { .. })),
        _ => false,
    }
}

/// Finalize phase of the fast worker: drain `score-queue/to_finalize/`. Each entry is
/// `{ submission, score }` produced by the eval worker. Per-job isolated — one
/// failure defers that job (left in the queue) without aborting the rest.
async fn finalize_scored_evals(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    model_catalog: &ModelCatalog,
    http_client: &reqwest::Client,
    stores: &Stores,
    chunk_limit: std::num::NonZeroUsize,
) -> anyhow::Result<usize> {
    let mut failed: HashSet<JobId> = HashSet::new();
    let mut finalized = 0usize;
    loop {
        let overhead = failed.len().min(FETCH_OVERHEAD_CAP);
        let fetch_limit = chunk_limit.saturating_add(overhead);
        let listed = stores
            .submissions
            .list_queue(ScoreQueueStage::ToFinalize, fetch_limit)
            .await?;
        let listed_len = listed.len();
        let chunk: Vec<JobId> = listed
            .into_iter()
            .filter(|j| !failed.contains(j))
            .take(chunk_limit.get())
            .collect();
        if chunk.is_empty() {
            break;
        }
        for job_id in chunk {
            match finalize_one(config, catalog, model_catalog, http_client, stores, &job_id).await {
                Ok(()) => finalized += 1,
                Err(e) => {
                    tracing::warn!(job_id = %job_id, error = %e, "finalize failed; deferring");
                    failed.insert(job_id);
                }
            }
        }
        if listed_len < fetch_limit.get() {
            break;
        }
    }
    Ok(finalized)
}

/// Finalize one scored eval job: derive from the stored response (no network),
/// write warehouse + eval rows, then archive to `processed/` and dequeue.
/// Archive-before-dequeue so a crash re-finalizes rather than dropping the
/// processed copy; all writes are idempotent.
async fn finalize_one(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    model_catalog: &ModelCatalog,
    http_client: &reqwest::Client,
    stores: &Stores,
    job_id: &JobId,
) -> anyhow::Result<()> {
    let payload = stores
        .submissions
        .read_queue(ScoreQueueStage::ToFinalize, job_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("to_finalize payload not found"))?;
    let submission_body = payload
        .get("submission")
        .ok_or_else(|| anyhow::anyhow!("to_finalize payload missing `submission`"))?;
    let score: scoring_service::ScoreResponse = serde_json::from_value(
        payload
            .get("score")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("to_finalize payload missing `score`"))?,
    )?;
    let parsed = crate::submission::parse_stored_submission(submission_body)
        .map_err(|e| anyhow::anyhow!("invalid submission in to_finalize: {e}"))?;
    let Submission::Success(success) = parsed else {
        anyhow::bail!("to_finalize submission is not a success body");
    };
    let scored = score_success(
        config,
        catalog,
        model_catalog,
        http_client,
        *success,
        Some(score),
    )
    .await?;

    if !scored.metric_rows.is_empty() {
        stores
            .warehouse
            .write_partition_metrics(
                &scored.benchmark_id,
                &scored.client_id,
                &scored.day_key,
                &scored.metric_rows,
            )
            .await?;
    }
    if let Some(esr) = &scored.eval_sample_results {
        stores.eval_sample_results.write(job_id, esr).await?;
    }
    stores
        .submissions
        .write_processed(job_id, submission_body)
        .await?;
    stores
        .submissions
        .dequeue(ScoreQueueStage::ToFinalize, job_id)
        .await?;
    Ok(())
}

/// Slow eval worker: drain `score-queue/to_do/`, call the scoring service, and
/// stage `{ submission, score }` into `score-queue/to_finalize/`. Runs on its
/// own schedule and takes no global mutate lock — it touches only the queue,
/// not the warehouse — so a multi-minute eval never blocks the fast worker.
pub async fn run_score_eval(config: &Config, stores: Stores) -> anyhow::Result<()> {
    let chunk_limit = config.score_chunk_size;
    let catalog = stores.catalog.load_catalog().await?;
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .build()?;

    let mut failed: HashSet<JobId> = HashSet::new();
    let mut scored = 0usize;
    let mut service_down = false;
    'outer: loop {
        let overhead = failed.len().min(FETCH_OVERHEAD_CAP);
        let fetch_limit = chunk_limit.saturating_add(overhead);
        let listed = stores
            .submissions
            .list_queue(ScoreQueueStage::ToDo, fetch_limit)
            .await?;
        let listed_len = listed.len();
        let chunk: Vec<JobId> = listed
            .into_iter()
            .filter(|j| !failed.contains(j))
            .take(chunk_limit.get())
            .collect();
        if chunk.is_empty() {
            break;
        }
        for job_id in chunk {
            match score_eval_one(config, &catalog, &stores, &http_client, &job_id).await {
                Ok(()) => scored += 1,
                Err(e) if is_service_unreachable(&e) => {
                    tracing::warn!(job_id = %job_id, error = %e, "scoring service unreachable; pausing eval worker");
                    service_down = true;
                    break 'outer;
                }
                Err(e) => {
                    tracing::error!(job_id = %job_id, error = %e, "failed to score eval submission");
                    failed.insert(job_id);
                }
            }
        }
        if listed_len < fetch_limit.get() {
            break;
        }
    }
    let suffix = if service_down {
        " Paused: scoring service unreachable; remaining to_do jobs retried next invocation."
    } else {
        ""
    };
    println!(
        "Eval scoring done: {scored} scored, {} failed.{suffix}",
        failed.len()
    );
    Ok(())
}

/// Score one `to_do` eval job and stage it for finalize. Idempotent: if it was
/// already scored (a `to_finalize` entry exists), drop the `to_do` marker
/// without re-paying the multi-minute service call.
async fn score_eval_one(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    stores: &Stores,
    http_client: &reqwest::Client,
    job_id: &JobId,
) -> anyhow::Result<()> {
    if stores
        .submissions
        .read_queue(ScoreQueueStage::ToFinalize, job_id)
        .await?
        .is_some()
    {
        stores
            .submissions
            .dequeue(ScoreQueueStage::ToDo, job_id)
            .await?;
        return Ok(());
    }

    let body = stores
        .submissions
        .read_queue(ScoreQueueStage::ToDo, job_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("to_do payload not found"))?;
    let parsed = crate::submission::parse_stored_submission(&body)
        .map_err(|e| anyhow::anyhow!("invalid submission in to_do: {e}"))?;
    let Submission::Success(success) = parsed else {
        anyhow::bail!("to_do submission is not a success body");
    };
    let benchmark = catalog
        .get(&success.wire.benchmark_id)
        .ok_or_else(|| anyhow::anyhow!("benchmark {} not in catalog", success.wire.benchmark_id))?;
    let (eval_id, dataset_name) = match &benchmark.def {
        BenchmarkDef::Eval {
            parameter_eval_id,
            parameter_dataset_name,
            ..
        } => (parameter_eval_id.as_str(), parameter_dataset_name.as_str()),
        _ => anyhow::bail!("to_do job {job_id} is not an eval benchmark"),
    };

    let completions = eval_completions(&success)?;
    let score =
        call_score_service(http_client, config, eval_id, dataset_name, &completions).await?;
    let payload = serde_json::json!({ "submission": body, "score": score });
    stores
        .submissions
        .enqueue(ScoreQueueStage::ToFinalize, job_id, &payload)
        .await?;
    stores
        .submissions
        .dequeue(ScoreQueueStage::ToDo, job_id)
        .await?;
    Ok(())
}

struct ChunkOutcome {
    outcomes: Vec<ScoreOutcome>,
    newly_failed: Vec<JobId>,
}

/// Whether an error from the scoring path means the scoring service was
/// unreachable (vs. a per-submission failure). The `ScoringError` is preserved
/// through the `anyhow` chain because no call site between here and
/// `scoring_service::score` re-wraps it, so a downcast recovers the variant.
fn is_service_unreachable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ScoringError>()
        .is_some_and(ScoringError::is_unreachable)
}

/// Score every submission in `chunk`. Per-job errors (read failure, score
/// failure) are collected into `newly_failed` so the caller can record them
/// for filtering future chunks; they never abort the run. Successfully
/// processed records (both scored and failure-typed) go into `outcomes`.
async fn score_chunk(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    model_catalog: &ModelCatalog,
    stores: &Stores,
    http_client: &reqwest::Client,
    chunk_idx: usize,
    chunk: &[JobId],
) -> ChunkOutcome {
    let chunk_size = chunk.len();
    let mut outcome = ChunkOutcome {
        outcomes: Vec::new(),
        newly_failed: Vec::new(),
    };

    for (i, job_id) in chunk.iter().enumerate() {
        let n = i + 1;
        let prefix = format!("[chunk {chunk_idx} {n}/{chunk_size}]");

        println!("{prefix} Scoring job {job_id}...");
        tracing::info!(
            progress = n,
            chunk_size,
            chunk = chunk_idx,
            job_id = %job_id,
            "scoring submission"
        );

        match read_and_score(config, catalog, model_catalog, stores, http_client, job_id).await {
            Ok(ScoreOutcome::Scored(scored)) => {
                println!("{prefix} Scored job {job_id}.");
                tracing::info!(
                    job_id = %job_id,
                    benchmark_id = %scored.benchmark_id,
                    client_id = %scored.client_id,
                    "scored submission"
                );
                outcome.outcomes.push(ScoreOutcome::Scored(scored));
            }
            Ok(ScoreOutcome::IgnoredFailure { job_id: jid }) => {
                println!("{prefix} Ignored failure submission for job {job_id} (not yet handled).");
                outcome
                    .outcomes
                    .push(ScoreOutcome::IgnoredFailure { job_id: jid });
            }
            Ok(ScoreOutcome::Routed { job_id: jid }) => {
                println!("{prefix} Routed eval job {job_id} to score-queue/to_do.");
                outcome.outcomes.push(ScoreOutcome::Routed { job_id: jid });
            }
            Err(e) => {
                outcome.newly_failed.push(job_id.clone());
                eprintln!("{prefix} Failed to score job {job_id}: {e}");
                tracing::error!(job_id = %job_id, error = %e, "failed to score submission");
            }
        }
    }

    outcome
}

/// Read the submission body and score it. Errors collapse the read-vs-score
/// distinction; the message preserves which step failed.
async fn read_and_score(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    model_catalog: &ModelCatalog,
    stores: &Stores,
    http_client: &reqwest::Client,
    job_id: &JobId,
) -> anyhow::Result<ScoreOutcome> {
    let record = stores
        .submissions
        .get_submission(job_id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read submission: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("submission not found"))?;

    // Eval jobs must never be scored on this fast path — that would make the
    // multi-minute /score call inside the mutate lock. Route them to
    // score-queue/to_do instead, where the slow `score-eval` pass picks them up.
    if submission_is_eval(catalog, &record.body) {
        stores
            .submissions
            .enqueue(ScoreQueueStage::ToDo, job_id, &record.body)
            .await?;
        stores.submissions.delete_incoming(job_id).await?;
        return Ok(ScoreOutcome::Routed {
            job_id: job_id.clone(),
        });
    }

    score_submission(config, catalog, model_catalog, http_client, &record).await
}

/// Result of attempting to score one submission. Success bodies
/// become a [`ScoredJob`] that the commit path persists to the
/// warehouse + eval_sample_results and then marks processed.
/// Failure-typed bodies (`message_type: "failure"`) are currently
/// ignored — the plans API that will give them meaning hasn't landed
/// yet, so the scorer leaves them in `incoming/` and skips both the
/// commit and the `mark_processed` transition. The
/// [`ScoreOutcome::IgnoredFailure`] variant carries just the `job_id`
/// so the per-run filter set can drop it from subsequent listings
/// without re-reading the body each tick.
enum ScoreOutcome {
    Scored(ScoredJob),
    IgnoredFailure {
        job_id: JobId,
    },
    /// An eval submission moved to `score-queue/to_do/` for the slow worker
    /// instead of being scored on this fast path. No commit needed — the body
    /// already left `incoming/`.
    Routed {
        job_id: JobId,
    },
}

/// Persist a chunk's outcomes. `IgnoredFailure` outcomes are skipped
/// throughout — those bodies stay in `incoming/`.
///
/// Warehouse metrics are written **batched per partition**; a failure there
/// aborts the run (a typed-row write doesn't fail per-item, so an error is
/// infrastructure — the existing loud-failure behaviour is preserved).
///
/// Eval sample results and `mark_processed` are then committed **per job in
/// isolation**: a job is marked only after its own eval results have landed,
/// and a per-job failure defers just that job (left in `incoming/`, returned
/// to the caller) instead of aborting — so one persistently-failing job can't
/// block the rest of the backlog.
///
/// Backstop: if *every* scored job fails to commit (e.g. the submissions store
/// is unreachable), that's systemic rather than a set of independent poison
/// pills, so it's surfaced as an error instead of a quietly "successful" run.
///
/// At-least-once: `mark_processed` is the last step per job, so a deferred or
/// crashed job is re-scored next run and the idempotent writes overwrite.
/// Returns the job_ids that were deferred (not marked processed).
async fn commit_chunk(stores: &Stores, outcomes: &[ScoreOutcome]) -> anyhow::Result<Vec<JobId>> {
    let scored: Vec<&ScoredJob> = outcomes
        .iter()
        .filter_map(|o| match o {
            ScoreOutcome::Scored(s) => Some(s),
            ScoreOutcome::IgnoredFailure { .. } | ScoreOutcome::Routed { .. } => None,
        })
        .collect();

    let partitions = scored.iter().fold(
        HashMap::<(BenchmarkId, ClientId, String), Vec<MetricRow>>::new(),
        |mut acc, job| {
            let key = (
                job.benchmark_id.clone(),
                job.client_id.clone(),
                job.day_key.clone(),
            );
            acc.entry(key)
                .or_default()
                .extend(job.metric_rows.iter().cloned());
            acc
        },
    );
    for ((benchmark_id, client_id, day_key), rows) in &partitions {
        tracing::debug!(
            benchmark_id = %benchmark_id,
            client_id = %client_id,
            day_key,
            rows = rows.len(),
            "writing partition metrics"
        );
        stores
            .warehouse
            .write_partition_metrics(benchmark_id, client_id, day_key, rows)
            .await?;
    }

    let mut deferred: Vec<JobId> = Vec::new();
    for job in &scored {
        if let Some(ref esr) = job.eval_sample_results {
            tracing::debug!(
                job_id = %job.job_id,
                rows = esr.len(),
                "writing eval sample results"
            );
            if let Err(e) = stores.eval_sample_results.write(&job.job_id, esr).await {
                tracing::warn!(
                    job_id = %job.job_id,
                    error = %e,
                    "eval sample results write failed; deferring job to next run"
                );
                deferred.push(job.job_id.clone());
                continue;
            }
        }
        if let Err(e) = stores.submissions.mark_processed(&job.job_id).await {
            tracing::warn!(
                job_id = %job.job_id,
                error = %e,
                "mark_processed failed; deferring job to next run"
            );
            deferred.push(job.job_id.clone());
        }
    }

    if !scored.is_empty() && deferred.len() == scored.len() {
        anyhow::bail!(
            "commit failed for all {} scored job(s) in chunk; submissions store likely unavailable",
            scored.len()
        );
    }

    Ok(deferred)
}

struct ScoredJob {
    job_id: JobId,
    benchmark_id: BenchmarkId,
    client_id: ClientId,
    day_key: String,
    metric_rows: Vec<MetricRow>,
    eval_sample_results: Option<Vec<EvalSampleResult>>,
}

async fn score_submission(
    config: &Config,
    catalog: &HashMap<BenchmarkId, crate::benchmark::Benchmark>,
    model_catalog: &ModelCatalog,
    http_client: &reqwest::Client,
    submission_record: &SubmissionRecord,
) -> anyhow::Result<ScoreOutcome> {
    if submission_record.state != JobState::Incoming {
        anyhow::bail!("submission {} is not incoming", submission_record.job_id);
    }

    // `parse_stored_submission` tolerates legacy bodies that lack
    // `message_type` (defaults to `"success"`). The handler always
    // injects the tag on new writes, but ~20k pre-existing bodies
    // in production lack it and the `fix-message-type` migration
    // wasn't run.
    let parsed = crate::submission::parse_stored_submission(&submission_record.body)
        .map_err(|e| anyhow::anyhow!("invalid submission body: {e}"))?;

    match parsed {
        Submission::Failure(f) => {
            tracing::info!(
                job_id = %f.job_id,
                benchmark_id = %f.wire.benchmark_id,
                client_id = %f.client_id,
                retriable = f.wire.retriable,
                failure_reason = ?f.wire.failure_reason.as_str(),
                "ignoring failure submission (not yet handled by scorer)"
            );
            Ok(ScoreOutcome::IgnoredFailure { job_id: f.job_id })
        }
        Submission::Success(success) => {
            score_success(config, catalog, model_catalog, http_client, *success, None)
                .await
                .map(ScoreOutcome::Scored)
        }
    }
}

async fn score_success(
    config: &Config,
    catalog: &HashMap<BenchmarkId, crate::benchmark::Benchmark>,
    model_catalog: &ModelCatalog,
    http_client: &reqwest::Client,
    submission: SuccessSubmission,
    eval_response: Option<scoring_service::ScoreResponse>,
) -> anyhow::Result<ScoredJob> {
    // `TrimmedString` fields are trimmed at deserialize, so no
    // explicit trim pass is needed here.
    let device_form_factor = submission
        .wire
        .device_form_factor
        .parse::<DeviceFormFactor>()
        .map_err(|e| {
            anyhow::anyhow!(
                "invalid device_form_factor {:?}: {e}",
                submission.wire.device_form_factor
            )
        })?;

    let benchmark_id = &submission.wire.benchmark_id;
    let client_id = &submission.client_id;

    let benchmark = catalog
        .get(benchmark_id)
        .ok_or_else(|| anyhow::anyhow!("benchmark {benchmark_id} not in catalog"))?;

    tracing::debug!(
        job_id = %submission.job_id,
        benchmark_id = %benchmark_id,
        client_id = %client_id,
        benchmark_type = benchmark.benchmark_type().as_ref(),
        "deriving metrics"
    );
    let scored_at = Utc::now();
    let derived =
        derive_metrics(benchmark, &submission, config, http_client, eval_response).await?;
    let metrics = &derived.metrics;
    tracing::debug!(
        job_id = %submission.job_id,
        benchmark_id = %benchmark_id,
        metric_count = metrics.len(),
        "derived metrics"
    );

    // Resolve total/active mill_params for the warehouse row. The
    // catalog wins over the submission value when the model is
    // recognized — see the comment on `resolve_mill_params` for
    // rationale and trade-offs.
    let (model_params_total_millions, model_params_active_millions) =
        resolve_mill_params(model_catalog, &submission);

    let day_key = warehouse::day_key_from_timestamp(submission.submitted_at.timestamp_micros())?;
    let benchmark_type = benchmark.benchmark_type();
    let submitted_at_us = submission.submitted_at.timestamp_micros();
    let scored_at_us = scored_at.timestamp_micros();

    // VL throughput records its measured token counts as observation columns
    // on every metric row (not as metrics). They come straight off the wire
    // and are None for every other benchmark type.
    let (obs_prefill_tokens, obs_image_tokens) = match benchmark.def {
        BenchmarkDef::VlThroughput { .. } => (
            submission.wire.prompt_tokens.map(|t| t as i32),
            submission.wire.image_tokens.map(|t| t.get() as i32),
        ),
        _ => (None, None),
    };

    // Canonicalize once (keys sorted, whitespace stripped); every metric row of
    // this submission carries the same denormalized model_descriptor / runtime_descriptor.
    let model_descriptor = submission
        .wire
        .model_descriptor
        .as_deref()
        .map(crate::canonical_json::canonicalize_str);
    let runtime_descriptor = submission
        .wire
        .runtime_descriptor
        .as_deref()
        .map(crate::canonical_json::canonicalize_str);
    // Denormalized onto every metric row alongside the descriptor it hashes.
    let model_descriptor_sha256 = model_descriptor
        .as_deref()
        .map(crate::canonical_json::sha256_hex);
    let runtime_descriptor_sha256 = runtime_descriptor
        .as_deref()
        .map(crate::canonical_json::sha256_hex);
    // Same treatment for the harness configuration: canonical form so pattern
    // search is stable, and a content id so "runs measured the same way" is a
    // group-by rather than a string comparison.
    let benchmark_flags =
        crate::canonical_json::canonicalize_flags(submission.wire.benchmark_flags.as_deref());
    let benchmark_flags_sha256 = benchmark_flags
        .as_deref()
        .map(crate::canonical_json::sha256_hex);
    // `model_flags` / `runtime_flags` get the same canonical form and content
    // id, with one difference from the fields above: they are documented to
    // accept a plain string (`--n-gpu-layers 999`) as well as JSON, and
    // `canonicalize_str` passes anything unparseable through trimmed. So the
    // JSON case groups stably and the plain-string case keeps working.
    let model_flags = crate::canonical_json::canonicalize_flags(
        submission.wire.model_flags.as_ref().map(AsRef::as_ref),
    );
    let runtime_flags = crate::canonical_json::canonicalize_flags(
        submission.wire.runtime_flags.as_ref().map(AsRef::as_ref),
    );
    let model_flags_sha256 = model_flags
        .as_deref()
        .map(crate::canonical_json::sha256_hex);
    let runtime_flags_sha256 = runtime_flags
        .as_deref()
        .map(crate::canonical_json::sha256_hex);

    let parquet_rows: Vec<MetricRow> = metrics
        .iter()
        .enumerate()
        .map(|(i, metric)| MetricRow {
            result_id: format!("{}_{i}", submission.job_id),
            benchmark_id: benchmark_id.clone(),
            benchmark_type,
            metric: metric.metric.clone(),
            client_id: client_id.clone(),
            device_name: submission.wire.device_name.clone().into(),
            device_form_factor,
            device_os_name: submission.wire.device_os_name.clone().into(),
            device_os_version: submission.wire.device_os_version.clone().into(),
            device_os_build: submission.wire.device_os_build.clone().map(Into::into),
            device_os_security_patch: submission
                .wire
                .device_os_security_patch
                .clone()
                .map(Into::into),
            device_chip_model: submission.wire.device_chip_model.clone().into(),
            device_gpu_model: submission.wire.device_gpu_model.clone().map(Into::into),
            device_gpu_vram_bytes: submission.wire.device_gpu_vram_bytes,
            device_npu_model: submission.wire.device_npu_model.clone().map(Into::into),
            device_npu_vram_bytes: submission.wire.device_npu_vram_bytes,
            device_ram_bytes: submission.wire.device_ram_bytes,
            device_battery_level: submission.wire.device_battery_level,
            device_power_state: submission.wire.device_power_state,
            device_power_save_mode: submission.wire.device_power_save_mode,
            device_android_cpuset: submission
                .wire
                .device_android_cpuset
                .clone()
                .map(Into::into),
            device_android_cpu_affinity_list: submission
                .wire
                .device_android_cpu_affinity_list
                .clone()
                .map(Into::into),
            device_android_cpu_affinity_excludes_top_tier: submission
                .wire
                .device_android_cpu_affinity_excludes_top_tier,
            device_apple_thermal_state_before: submission
                .wire
                .device_apple_thermal_state_before
                .clone(),
            device_apple_thermal_state_after: submission
                .wire
                .device_apple_thermal_state_after
                .clone(),
            device_apple_soc_temp_c_before: submission.wire.device_apple_soc_temp_c_before.clone(),
            device_apple_soc_temp_c_after: submission.wire.device_apple_soc_temp_c_after.clone(),
            device_android_thermal_status_before: submission
                .wire
                .device_android_thermal_status_before
                .clone(),
            device_android_thermal_status_after: submission
                .wire
                .device_android_thermal_status_after
                .clone(),
            device_android_thermal_headroom_before: submission
                .wire
                .device_android_thermal_headroom_before
                .clone(),
            device_android_thermal_headroom_after: submission
                .wire
                .device_android_thermal_headroom_after
                .clone(),
            device_android_thermal_sensors_before: submission
                .wire
                .device_android_thermal_sensors_before
                .clone(),
            device_android_thermal_sensors_after: submission
                .wire
                .device_android_thermal_sensors_after
                .clone(),
            device_linux_thermal_zones_before: submission
                .wire
                .device_linux_thermal_zones_before
                .clone(),
            device_linux_thermal_zones_after: submission
                .wire
                .device_linux_thermal_zones_after
                .clone(),
            model_name: submission.wire.model_name.clone().map(Into::into),
            model_quant: submission.wire.model_quant.clone().map(Into::into),
            model_params_total_millions,
            model_params_active_millions,
            model_flags: model_flags.clone(),
            model_flags_sha256: model_flags_sha256.clone(),
            runtime_name: submission.wire.runtime_name.clone().map(Into::into),
            runtime_version: submission.wire.runtime_version.clone().map(Into::into),
            runtime_flags: runtime_flags.clone(),
            runtime_flags_sha256: runtime_flags_sha256.clone(),
            runtime_cpu_variant: submission.wire.runtime_cpu_variant.clone().map(Into::into),
            client_version: submission.wire.client_version.clone().map(Into::into),
            value: metric.value,
            value_stddev: metric.value_stddev,
            unit: metric.unit.clone(),
            submitted_at: submitted_at_us,
            scored_at: scored_at_us,
            parameter_prefill_tokens: get_param_prefill_tokens(benchmark),
            parameter_decode_tokens: get_param_decode_tokens(benchmark),
            parameter_eval_id: get_param_eval_id(benchmark),
            parameter_image_width: get_param_image_width(benchmark),
            parameter_image_height: get_param_image_height(benchmark),
            parameter_text_tokens: get_param_text_tokens(benchmark),
            parameter_num_images: get_param_num_images(benchmark),
            observation_vl_throughput_prefill_tokens: obs_prefill_tokens,
            observation_vl_throughput_image_tokens: obs_image_tokens,
            score_runtime_version: derived.score_runtime_version.clone(),
            // Every MetricRow of this submission carries the same
            // eval_metadata blob — denormalized like the device /
            // model / runtime fields, so a single warehouse row stands
            // alone without a join.
            eval_metadata: derived.eval_metadata.clone(),
            model_descriptor: model_descriptor.clone(),
            runtime_descriptor: runtime_descriptor.clone(),
            model_descriptor_sha256: model_descriptor_sha256.clone(),
            runtime_descriptor_sha256: runtime_descriptor_sha256.clone(),
            benchmark_flags: benchmark_flags.clone(),
            benchmark_flags_sha256: benchmark_flags_sha256.clone(),
        })
        .collect();

    Ok(ScoredJob {
        job_id: submission.job_id.clone(),
        benchmark_id: benchmark_id.clone(),
        client_id: client_id.clone(),
        day_key,
        metric_rows: parquet_rows,
        eval_sample_results: derived.eval_sample_results,
    })
}

/// Resolve `(total, active)` `model_params_*_millions` for the warehouse row.
///
/// The model is resolved against the curated catalog by `model_name` first,
/// then — when `model_name` is absent or unrecognized — by a substring match
/// against the opaque `model_descriptor`. When it resolves, the canonical
/// `(total, active)` wins, even if the submission carried different numbers —
/// that's how stale or wrong client-provided values get corrected over time.
/// When it doesn't resolve the submission's values are used; the active value
/// falls back to `total` when the submission omits it (the dense-model case).
///
/// The resolution is logged at `info` when it changes the stored
/// value, so ops have visibility without spam in the steady-state case
/// where the submission already matches the catalog.
fn resolve_mill_params(
    catalog: &ModelCatalog,
    submission: &SuccessSubmission,
) -> (Option<i32>, Option<i32>) {
    // Prefer an exact `model_name` lookup; fall back to a substring match
    // against the opaque `model_descriptor` when `model_name` is absent (or
    // unrecognized). `model_name` is now optional, so descriptor-first
    // submissions still get the catalog correction.
    let resolved = submission
        .wire
        .model_name
        .as_ref()
        .and_then(|name| catalog.lookup(name).map(|e| (name.as_str().to_owned(), e)))
        .or_else(|| {
            submission.wire.model_descriptor.as_deref().and_then(|d| {
                catalog
                    .resolve_from_descriptor(d)
                    .map(|e| ("model_descriptor".to_owned(), e))
            })
        });

    if let Some((resolved_via, entry)) = resolved {
        let submission_total = submission.wire.model_params_total_millions;
        let submission_active = submission
            .wire
            .model_params_active_millions
            .or(submission_total);
        if submission_total != Some(entry.total) || submission_active != Some(entry.active) {
            tracing::info!(
                job_id = %submission.job_id,
                resolved_via = %resolved_via,
                submission_total = ?submission_total,
                submission_active = ?submission_active,
                catalog_total = entry.total,
                catalog_active = entry.active,
                "scorer normalizing mill_params to catalog values"
            );
        }
        return (Some(entry.total), Some(entry.active));
    }
    // Unknown model: trust the submission. Fall back to total for
    // active so dense-model rows always have both columns populated.
    let total = submission.wire.model_params_total_millions;
    let active = submission.wire.model_params_active_millions.or(total);
    (total, active)
}

struct DerivedMetric {
    metric: String,
    value: f32,
    value_stddev: Option<f32>,
    unit: String,
}

struct DerivedResult {
    metrics: Vec<DerivedMetric>,
    eval_sample_results: Option<Vec<EvalSampleResult>>,
    score_runtime_version: Option<String>,
    /// JSON-encoded `{key: value}` map mirrored onto every MetricRow of
    /// this submission via `MetricRow.eval_metadata`. Currently
    /// carries `{"samples_failed": N}` for eval submissions where any
    /// sample crashed client-side. `None` when there's nothing to
    /// record (no failed samples, or a non-eval benchmark).
    eval_metadata: Option<String>,
}

fn require<T>(value: Option<T>, field: &str) -> anyhow::Result<T> {
    value.ok_or_else(|| anyhow::anyhow!("missing {field}"))
}

/// The completions to score from an eval submission: drops duplicate ids
/// (first occurrence wins — a legacy safety net before the gateway rejected
/// dupes) and bails when there are none to score (rather than emitting a
/// misleading `accuracy = 0/0 → 0.0`). Shared by the inline derive path and
/// the slow eval worker.
fn eval_completions(submission: &SuccessSubmission) -> anyhow::Result<Vec<SampleCompletion>> {
    let completions = submission
        .wire
        .completions
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing completions"))?;

    let mut seen: HashSet<&str> = HashSet::with_capacity(completions.len());
    let unique_count = completions
        .iter()
        .filter(|c| seen.insert(c.id.as_str()))
        .count();
    let deduped: Vec<SampleCompletion> = if unique_count == completions.len() {
        completions.to_vec()
    } else {
        tracing::warn!(
            job_id = %submission.job_id,
            original = completions.len(),
            unique = unique_count,
            "deduping duplicate completion ids before scoring (legacy submission); keeping first occurrence"
        );
        seen.clear();
        completions
            .iter()
            .filter(|c| seen.insert(c.id.as_str()))
            .cloned()
            .collect()
    };

    if deduped.is_empty() {
        anyhow::bail!("eval submission has no completions to score");
    }
    Ok(deduped)
}

/// Build the `/score` request from `completions` and call the scoring service.
/// This is the only multi-minute, network-bound step in the eval pipeline; the
/// slow worker runs it, and the inline derive path uses it when no response was
/// pre-fetched. Every completion (including client-`failed` ones, which carry
/// an empty `completion`) is forwarded; only the non-contract `failed` /
/// `failed_reason` fields are stripped.
async fn call_score_service(
    http_client: &reqwest::Client,
    config: &Config,
    eval_id: &str,
    dataset_name: &str,
    completions: &[SampleCompletion],
) -> anyhow::Result<scoring_service::ScoreResponse> {
    let score_samples: Vec<ScoreRequestSample<'_>> = completions
        .iter()
        .map(|c| ScoreRequestSample {
            id: &c.id,
            completion: &c.completion,
        })
        .collect();
    tracing::info!(
        eval_id = %eval_id,
        dataset = %dataset_name,
        total = completions.len(),
        samples_failed = completions.iter().filter(|c| is_failed_sample(c)).count(),
        "calling evals server"
    );
    // `?` converts `ScoringError` into the anyhow chain while preserving it as
    // the source, so `is_service_unreachable` can still downcast it.
    Ok(scoring_service::score(
        http_client,
        &config.evals_server_url,
        &ScoreRequest {
            eval_id,
            dataset_name,
            completions: &score_samples,
        },
    )
    .await?)
}

/// Derive metrics from a submission based on its benchmark type.
///
/// For `Eval`, `eval_response` lets the caller supply an already-fetched
/// scoring-service response (the fast finalize stage reads it from the
/// `to_finalize` payload) so the multi-minute `/score` call is not repeated;
/// `None` calls the service inline (the slow path and the non-split callers).
/// Ignored for non-eval benchmark types.
async fn derive_metrics(
    benchmark: &Benchmark,
    submission: &SuccessSubmission,
    config: &Config,
    http_client: &reqwest::Client,
    eval_response: Option<scoring_service::ScoreResponse>,
) -> anyhow::Result<DerivedResult> {
    match &benchmark.def {
        BenchmarkDef::PrefillThroughput {
            parameter_prefill_tokens,
            ..
        } => {
            let prefill_time_ms = require(submission.wire.prefill_time_ms, "prefill_time_ms")?;
            let stddev = submission.wire.prefill_time_ms_stddev;
            let throughput = *parameter_prefill_tokens as f32 / prefill_time_ms * 1000.0;
            let throughput_stddev = stddev.map(|s| throughput * s / prefill_time_ms);
            Ok(DerivedResult {
                metrics: vec![
                    DerivedMetric {
                        metric: "ttft".to_string(),
                        value: prefill_time_ms,
                        value_stddev: stddev,
                        unit: "ms".to_string(),
                    },
                    DerivedMetric {
                        metric: "prefill_throughput".to_string(),
                        value: throughput,
                        value_stddev: throughput_stddev,
                        unit: "tokens/sec".to_string(),
                    },
                ],
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDef::DecodeThroughput {
            parameter_decode_tokens,
            ..
        } => {
            let decode_time_ms = require(submission.wire.decode_time_ms, "decode_time_ms")?;
            let stddev = submission.wire.decode_time_ms_stddev;
            let throughput = *parameter_decode_tokens as f32 / decode_time_ms * 1000.0;
            let throughput_stddev = stddev.map(|s| throughput * s / decode_time_ms);
            Ok(DerivedResult {
                metrics: vec![DerivedMetric {
                    metric: "decode_throughput".to_string(),
                    value: throughput,
                    value_stddev: throughput_stddev,
                    unit: "tokens/sec".to_string(),
                }],
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDef::EndToEndLatency { .. } => {
            let total_time_ms = require(submission.wire.total_time_ms, "total_time_ms")?;
            Ok(DerivedResult {
                metrics: vec![DerivedMetric {
                    metric: "end_to_end_latency".to_string(),
                    value: total_time_ms,
                    value_stddev: submission.wire.total_time_ms_stddev,
                    unit: "ms".to_string(),
                }],
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDef::MaxMemoryUsage { .. } | BenchmarkDef::VlMaxMemory { .. } => {
            let max_host = require(submission.wire.max_host_bytes, "max_host_bytes")? as f32;
            let mut metrics = vec![DerivedMetric {
                metric: "max_host_usage".to_string(),
                value: max_host,
                value_stddev: None,
                unit: "bytes".to_string(),
            }];
            metrics.extend(
                [
                    (submission.wire.max_gpu_bytes, "max_gpu_usage"),
                    (submission.wire.max_npu_bytes, "max_npu_usage"),
                ]
                .into_iter()
                .filter_map(|(opt, name)| {
                    opt.map(|v| DerivedMetric {
                        metric: name.to_string(),
                        value: v as f32,
                        value_stddev: None,
                        unit: "bytes".to_string(),
                    })
                }),
            );
            Ok(DerivedResult {
                metrics,
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDef::Eval {
            parameter_eval_id,
            parameter_dataset_name,
            ..
        } => {
            let completions = eval_completions(submission)?;
            let samples_failed = completions.iter().filter(|c| is_failed_sample(c)).count();

            // Use a pre-fetched response (fast finalize) or call the scoring
            // service inline (slow path / non-split callers).
            let body = match eval_response {
                Some(body) => body,
                None => {
                    call_score_service(
                        http_client,
                        config,
                        parameter_eval_id,
                        parameter_dataset_name,
                        &completions,
                    )
                    .await?
                }
            };

            // Verify the scorer's response is in one-to-one correspondence
            // with what we forwarded: drop the original `failed` /
            // `failed_reason` metadata from every completion onto the
            // matching scored row, and bail if anything is missing or
            // extra. See `build_eval_sample_results` for the contract.
            let eval_sample_results = build_eval_sample_results(&completions, body.scored_samples)?;

            let total = eval_sample_results.len();
            let correct = eval_sample_results.iter().filter(|r| r.is_correct).count();
            // total > 0 is guaranteed because the empty-completions case
            // bails above and the scorer must echo every id (verified by
            // `build_eval_sample_results`).
            let accuracy = correct as f32 / total as f32;
            let context_json = serde_json::to_string(&body.context)
                .expect("BTreeMap<String, Value> serializes infallibly");
            tracing::info!(
                eval_id = %parameter_eval_id,
                correct,
                total,
                samples_failed,
                accuracy,
                runtime_version = ?body.runtime_version,
                context = %context_json,
                "eval scored"
            );

            // `accuracy` is computed over **all** scored samples — failed
            // ones are scored as wrong by the scorer because of the empty
            // completion. Consumers that want the "accuracy over samples
            // we could actually evaluate" can derive it themselves from
            // the `samples_failed` entry in `eval_metadata`. mgmt
            // just records the raw numbers; it does not pre-compute
            // alternative accuracy variants.
            let metrics = vec![DerivedMetric {
                metric: "accuracy".to_string(),
                value: accuracy,
                value_stddev: None,
                unit: "ratio".to_string(),
            }];
            // Per-run metadata that doesn't belong on the metric axis goes
            // into the warehouse row's `eval_metadata` column. The blob is
            // denormalized onto every metric row of the submission below
            // (see `parquet_rows` mapper) — matches the existing pattern
            // for device / model / runtime fields and keeps each row
            // self-describing. Today this is one row per eval submission
            // so there's no duplication in practice.
            let eval_metadata = build_eval_metadata(samples_failed);

            Ok(DerivedResult {
                metrics,
                eval_sample_results: Some(eval_sample_results),
                score_runtime_version: Some(body.runtime_version),
                eval_metadata,
            })
        }
        BenchmarkDef::VlThroughput {
            parameter_decode_tokens,
            ..
        } => {
            let prompt_tokens = require(submission.wire.prompt_tokens, "prompt_tokens")? as f32;
            let prompt_ms = require(submission.wire.prompt_ms, "prompt_ms")?;
            let predicted_ms = require(submission.wire.predicted_ms, "predicted_ms")?;
            let prompt_stddev = submission.wire.prompt_ms_stddev;
            let predicted_stddev = submission.wire.predicted_ms_stddev;

            let prefill_throughput = prompt_tokens / prompt_ms * 1000.0;
            let decode_throughput = *parameter_decode_tokens as f32 / predicted_ms * 1000.0;

            let prefill_tp_stddev = prompt_stddev.map(|s| prefill_throughput * s / prompt_ms);
            let decode_tp_stddev = predicted_stddev.map(|s| decode_throughput * s / predicted_ms);
            let e2e_stddev = match (prompt_stddev, predicted_stddev) {
                (Some(sp), Some(sd)) => Some((sp * sp + sd * sd).sqrt()),
                (Some(s), None) | (None, Some(s)) => Some(s),
                (None, None) => None,
            };

            let metrics = vec![
                DerivedMetric {
                    metric: "ttft".to_string(),
                    value: prompt_ms,
                    value_stddev: prompt_stddev,
                    unit: "ms".to_string(),
                },
                DerivedMetric {
                    metric: "prefill_throughput".to_string(),
                    value: prefill_throughput,
                    value_stddev: prefill_tp_stddev,
                    unit: "tokens/sec".to_string(),
                },
                DerivedMetric {
                    metric: "decode_throughput".to_string(),
                    value: decode_throughput,
                    value_stddev: decode_tp_stddev,
                    unit: "tokens/sec".to_string(),
                },
                DerivedMetric {
                    metric: "e2e_latency".to_string(),
                    value: prompt_ms + predicted_ms,
                    value_stddev: e2e_stddev,
                    unit: "ms".to_string(),
                },
            ];
            // The prefill length (image + text + template) and the image-only
            // portion are recorded as observation columns on every metric row
            // (see `observation_vl_throughput_*` in the warehouse), not as
            // metrics: they are measured workload facts, not performance
            // results. The scored row-construction reads them off the wire.

            Ok(DerivedResult {
                metrics,
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
    }
}

/// Whether a completion counts toward the `samples_failed` metric. `stop_reason
/// == "failure"` is the sole signal — the legacy `failed` flag is not consulted.
fn is_failed_sample(c: &SampleCompletion) -> bool {
    c.stop_reason.as_deref() == Some("failure")
}

/// Re-inject the client-side per-sample metadata onto each `ScoredSample`
/// returned by the scoring service, returning one `EvalSampleResult` per
/// row in `scored_samples`. Besides `failed` / `failed_reason`, this
/// carries the `stop_reason` plumbing:
///
/// * the client's captured `stop_reason` is the sole source of truth; the
///   legacy `failed` flag is not consulted;
/// * the retiring `failed` / `failed_reason` columns are derived from
///   `stop_reason == failure` so they can't disagree with the reason;
/// * `stop_reason_source = recorded` whenever a `stop_reason` is set (the
///   client observed it) — a `derived` source is reserved for after-the-fact
///   backfill;
/// * `stop_detail` passes through from the client, falling back to
///   `failed_reason` on a failure so the crash detail is never lost;
/// * `completion_tokens` passes through from the client.
///
/// **Verification contract**: every `id` in `completions` must appear
/// in `scored_samples` exactly once, and no extra ids may be present.
/// If the scorer dropped a row or returned an unknown id, `bail!` so
/// the job stays in `incoming/` for human investigation — silently
/// patching the response would mask a real contract violation.
fn build_eval_sample_results(
    completions: &[SampleCompletion],
    scored_samples: Vec<scoring_service::ScoredSample>,
) -> anyhow::Result<Vec<EvalSampleResult>> {
    let mut unmatched: HashMap<&str, &SampleCompletion> =
        completions.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut unknown: Vec<String> = Vec::new();

    let results: Vec<EvalSampleResult> = scored_samples
        .into_iter()
        .map(|s| {
            let orig = unmatched.remove(s.id.as_str());
            let (failed, failed_reason, stop_reason, stop_detail, completion_tokens) = match orig {
                Some(c) => {
                    // `stop_reason` is the sole source of truth for how the
                    // sample ended; the legacy `failed` flag is not consulted.
                    let stop_reason = c.stop_reason.clone();
                    let is_failure = stop_reason.as_deref() == Some("failure");
                    // `stop_detail` prefers the client's value, falling back to
                    // `failed_reason` on a failure so the crash detail survives
                    // for clients that predate `stop_detail`.
                    let stop_detail = if is_failure {
                        c.stop_detail.clone().or_else(|| c.failed_reason.clone())
                    } else {
                        c.stop_detail.clone()
                    };
                    // The retiring `failed` / `failed_reason` columns are derived
                    // from `stop_reason == failure`, not copied from the flag, so
                    // they can't disagree with the reason.
                    let failed_reason = if is_failure {
                        c.failed_reason.clone()
                    } else {
                        None
                    };
                    (
                        is_failure,
                        failed_reason,
                        stop_reason,
                        stop_detail,
                        c.completion_tokens,
                    )
                }
                None => {
                    // Scorer echoed an id we never sent. Record it for
                    // the post-loop check; keep building the row so the
                    // bail message has the full picture.
                    unknown.push(s.id.clone().into_inner());
                    (false, None, None, None, None)
                }
            };
            // `recorded` = the client observed this stop_reason at generation.
            // `derived` is left for backfill jobs.
            let stop_reason_source = stop_reason.as_ref().map(|_| "recorded".to_string());
            Ok(EvalSampleResult {
                id: s.id.into_inner(),
                messages: serde_json::to_string(&s.messages)?,
                completion: s.completion,
                is_correct: s.is_correct,
                failed,
                failed_reason,
                stop_reason,
                stop_reason_source,
                stop_detail,
                completion_tokens,
            })
        })
        .collect::<anyhow::Result<_>>()?;

    if !unmatched.is_empty() || !unknown.is_empty() {
        let mut dropped_ids: Vec<&str> = unmatched.keys().copied().collect();
        dropped_ids.sort_unstable();
        unknown.sort();
        anyhow::bail!(
            "scorer response mismatch: dropped {:?}, unknown {:?}",
            dropped_ids,
            unknown
        );
    }

    Ok(results)
}

/// Build the warehouse row's `eval_metadata` JSON object from per-run
/// counters. Returns `None` when there's nothing to record, so we don't
/// emit empty `{}` blobs into the warehouse parquet. Using a
/// `serde_json::Map` (rather than chained `json!` macros) keeps the
/// shape extensible — future keys can be added with a single
/// `insert` call.
fn build_eval_metadata(samples_failed: usize) -> Option<String> {
    let mut map = serde_json::Map::new();
    if samples_failed > 0 {
        map.insert(
            "samples_failed".to_string(),
            serde_json::Value::from(samples_failed),
        );
    }
    (!map.is_empty()).then(|| serde_json::Value::Object(map).to_string())
}

fn get_param_prefill_tokens(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDef::PrefillThroughput {
            parameter_prefill_tokens,
            ..
        }
        | BenchmarkDef::DecodeThroughput {
            parameter_prefill_tokens,
            ..
        }
        | BenchmarkDef::EndToEndLatency {
            parameter_prefill_tokens,
            ..
        }
        | BenchmarkDef::MaxMemoryUsage {
            parameter_prefill_tokens,
            ..
        } => Some(*parameter_prefill_tokens),
        BenchmarkDef::Eval { .. }
        | BenchmarkDef::VlThroughput { .. }
        | BenchmarkDef::VlMaxMemory { .. } => None,
    }
}

fn get_param_decode_tokens(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDef::DecodeThroughput {
            parameter_decode_tokens,
            ..
        }
        | BenchmarkDef::EndToEndLatency {
            parameter_decode_tokens,
            ..
        }
        | BenchmarkDef::VlThroughput {
            parameter_decode_tokens,
            ..
        } => Some(*parameter_decode_tokens),
        _ => None,
    }
}

fn get_param_eval_id(benchmark: &Benchmark) -> Option<String> {
    match &benchmark.def {
        BenchmarkDef::Eval {
            parameter_eval_id, ..
        } => Some(parameter_eval_id.clone()),
        _ => None,
    }
}

fn get_param_image_width(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDef::VlThroughput {
            parameter_image_width,
            ..
        }
        | BenchmarkDef::VlMaxMemory {
            parameter_image_width,
            ..
        } => Some(*parameter_image_width),
        _ => None,
    }
}

fn get_param_image_height(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDef::VlThroughput {
            parameter_image_height,
            ..
        }
        | BenchmarkDef::VlMaxMemory {
            parameter_image_height,
            ..
        } => Some(*parameter_image_height),
        _ => None,
    }
}

fn get_param_text_tokens(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDef::VlThroughput {
            parameter_text_tokens,
            ..
        }
        | BenchmarkDef::VlMaxMemory {
            parameter_text_tokens,
            ..
        } => Some(*parameter_text_tokens),
        _ => None,
    }
}

fn get_param_num_images(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDef::VlThroughput {
            parameter_num_images,
            ..
        }
        | BenchmarkDef::VlMaxMemory {
            parameter_num_images,
            ..
        } => Some(*parameter_num_images),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::benchmark::Benchmark;
    use crate::score::*;
    use anyhow::Context;
    use serde_json::{Value, json};

    fn test_submission(extras: Value) -> SuccessSubmission {
        let mut base = json!({
            "benchmark_id": "test",
            "client_id": "test",
            "job_id": "test",
            "submitted_at": "2026-01-01T00:00:00Z",
            "benchmark_type": "prefill_throughput",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 16_000_000_000_i64,
            "model_name": "test-model",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "test-runtime",
            "runtime_version": "v1",
        });
        if let Value::Object(map) = extras {
            base.as_object_mut().unwrap().extend(map);
        }
        serde_json::from_value(base).expect("test_submission should deserialize")
    }

    #[tokio::test]
    async fn test_derive_prefill_metrics() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_256")?,
            def: BenchmarkDef::PrefillThroughput {
                parameter_prefill_tokens: 256,
            },
        };
        let submission = test_submission(json!({
            "prefill_time_ms": 34.7,
            "prefill_time_ms_stddev": 1.25
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].metric, "ttft");
        assert!((metrics[0].value - 34.7).abs() < 0.1);
        assert_eq!(metrics[0].value_stddev, Some(1.25));
        assert_eq!(metrics[1].metric, "prefill_throughput");
        // 256 / 34.7 * 1000 ≈ 7378.1
        assert!((metrics[1].value - 7378.1).abs() < 1.0);
        // throughput_stddev = throughput * stddev / time = 7378.1 * 1.25 / 34.7 ≈ 265.7
        assert!(metrics[1].value_stddev.is_some());
        let stddev = metrics[1].value_stddev.context("expected stddev")?;
        assert!((stddev - 265.7).abs() < 1.0);
        Ok(())
    }

    #[tokio::test]
    async fn test_derive_decode_metrics() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("decode_throughput_512_100")?,
            def: BenchmarkDef::DecodeThroughput {
                parameter_prefill_tokens: 512,
                parameter_decode_tokens: 100,
            },
        };
        let submission = test_submission(json!({
            "decode_time_ms": 50.0,
            "decode_time_ms_stddev": 2.0
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].metric, "decode_throughput");
        // 100 / 50.0 * 1000 = 2000
        assert!((metrics[0].value - 2000.0).abs() < 0.1);
        // throughput_stddev = 2000 * 2.0 / 50.0 = 80.0
        assert_eq!(metrics[0].value_stddev, Some(80.0));
        Ok(())
    }

    #[tokio::test]
    async fn test_derive_e2e_latency_metrics() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("end_to_end_latency_256_256")?,
            def: BenchmarkDef::EndToEndLatency {
                parameter_prefill_tokens: 256,
                parameter_decode_tokens: 256,
            },
        };
        let submission = test_submission(json!({ "total_time_ms": 123.4 }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].metric, "end_to_end_latency");
        assert!((metrics[0].value - 123.4).abs() < 0.1);
        assert_eq!(metrics[0].value_stddev, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_derive_memory_metrics() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("max_memory_usage_256")?,
            def: BenchmarkDef::MaxMemoryUsage {
                parameter_prefill_tokens: 256,
            },
        };
        let submission = test_submission(json!({
            "max_host_bytes": 1073741824_i64,
            "max_gpu_bytes": 536870912_i64,
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].metric, "max_host_usage");
        assert_eq!(metrics[0].value_stddev, None);
        assert_eq!(metrics[1].metric, "max_gpu_usage");
        assert_eq!(metrics[1].value_stddev, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_derive_vl_max_memory_metrics() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("vl_max_memory_512x512_t0_f1")?,
            def: BenchmarkDef::VlMaxMemory {
                parameter_image_width: 512,
                parameter_image_height: 512,
                parameter_text_tokens: 0,
                parameter_num_images: 1,
            },
        };
        let submission = test_submission(json!({
            "max_host_bytes": 2147483648_i64,
            "max_gpu_bytes": 1073741824_i64,
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].metric, "max_host_usage");
        assert!((metrics[0].value - 2147483648.0).abs() < 1.0);
        assert_eq!(metrics[1].metric, "max_gpu_usage");
        assert!((metrics[1].value - 1073741824.0).abs() < 1.0);
        Ok(())
    }

    #[tokio::test]
    async fn test_max_memory_usage_without_gpu() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("max_memory_usage_256")?,
            def: BenchmarkDef::MaxMemoryUsage {
                parameter_prefill_tokens: 256,
            },
        };
        let submission = test_submission(json!({
            "max_host_bytes": 1073741824_i64,
            "max_gpu_bytes": null,
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].metric, "max_host_usage");
        assert_eq!(metrics[0].value_stddev, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_max_memory_usage_without_gpu_field() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("max_memory_usage_256")?,
            def: BenchmarkDef::MaxMemoryUsage {
                parameter_prefill_tokens: 256,
            },
        };
        let submission = test_submission(json!({
            "max_host_bytes": 1073741824_i64,
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].metric, "max_host_usage");
        assert_eq!(metrics[0].value_stddev, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_derive_vl_throughput_metrics() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("vl_throughput_384x512_32_128")?,
            def: BenchmarkDef::VlThroughput {
                parameter_image_width: 384,
                parameter_image_height: 512,
                parameter_text_tokens: 32,
                parameter_decode_tokens: 128,
                parameter_num_images: 1,
            },
        };
        let submission = test_submission(json!({
            "prompt_tokens": 224,
            "image_tokens": 192,
            "prompt_ms": 50.0,
            "prompt_ms_stddev": 2.5,
            "predicted_ms": 200.0,
            "predicted_ms_stddev": 10.0
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        // Four performance metrics; prefill/image token counts are recorded as
        // observation columns on the scored row, not as metrics.
        assert_eq!(metrics.len(), 4);

        assert_eq!(metrics[0].metric, "ttft");
        assert!((metrics[0].value - 50.0).abs() < 0.1);
        assert_eq!(metrics[0].value_stddev, Some(2.5));
        assert_eq!(metrics[0].unit, "ms");

        assert_eq!(metrics[1].metric, "prefill_throughput");
        // 224 / 50.0 * 1000 = 4480.0
        assert!((metrics[1].value - 4480.0).abs() < 1.0);
        // prefill_tp_stddev = 4480.0 * 2.5 / 50.0 = 224.0
        let prefill_stddev = metrics[1].value_stddev.context("expected prefill stddev")?;
        assert!((prefill_stddev - 224.0).abs() < 1.0);
        assert_eq!(metrics[1].unit, "tokens/sec");

        assert_eq!(metrics[2].metric, "decode_throughput");
        // 128 / 200.0 * 1000 = 640.0
        assert!((metrics[2].value - 640.0).abs() < 1.0);
        // decode_tp_stddev = 640.0 * 10.0 / 200.0 = 32.0
        let decode_stddev = metrics[2].value_stddev.context("expected decode stddev")?;
        assert!((decode_stddev - 32.0).abs() < 1.0);
        assert_eq!(metrics[2].unit, "tokens/sec");

        assert_eq!(metrics[3].metric, "e2e_latency");
        assert!((metrics[3].value - 250.0).abs() < 0.1);
        // e2e_stddev = sqrt(2.5^2 + 10.0^2) = sqrt(106.25) ≈ 10.308
        let e2e_stddev = metrics[3].value_stddev.context("expected e2e stddev")?;
        assert!((e2e_stddev - 10.308).abs() < 0.01);
        assert_eq!(metrics[3].unit, "ms");
        Ok(())
    }

    // ---- build_eval_metadata -------------------------------------------

    #[tokio::test]
    async fn derive_metrics_bails_on_empty_eval_completions() -> anyhow::Result<()> {
        // Empty completions must bail BEFORE the /score HTTP call, so the
        // test doesn't need a reachable scoring service to succeed: if the
        // bail regresses we'd see a network error (or accuracy = 0/0 = 0.0)
        // instead of the contract-violation message asserted below.
        let benchmark = Benchmark {
            benchmark_id: BenchmarkId::try_new("eval_bench")?,
            def: BenchmarkDef::Eval {
                parameter_eval_id: "test_eval".to_string(),
                parameter_dataset_name: "default".to_string(),
                parameter_max_tokens: 100,
                parameter_mcq_choices: None,
            },
        };
        let submission = test_submission(json!({
            "benchmark_id": "eval_bench",
            "benchmark_type": "eval",
            "completions": [],
        }));
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = reqwest::Client::new();
        let result = derive_metrics(&benchmark, &submission, &config, &client, None).await;
        let err = match result {
            Ok(_) => panic!("expected bail on empty completions"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("no completions"),
            "unexpected error message: {msg}"
        );
        Ok(())
    }

    #[test]
    fn build_eval_metadata_returns_none_when_no_failures() {
        assert_eq!(build_eval_metadata(0), None);
    }

    #[test]
    fn build_eval_metadata_emits_samples_failed_when_positive() {
        let s = build_eval_metadata(3).expect("expected metadata");
        assert_eq!(s, r#"{"samples_failed":3}"#);
    }

    // ---- build_eval_sample_results -------------------------------------

    fn scored(id: &str, is_correct: bool) -> anyhow::Result<scoring_service::ScoredSample> {
        Ok(scoring_service::ScoredSample {
            id: crate::validated::NonEmptyTrimmedString::try_new(id)?,
            messages: vec![scoring_service::ChatMessage {
                role: "user".to_string(),
                content: format!("prompt for {id}"),
                extra: Default::default(),
            }],
            completion: if is_correct {
                "B".to_string()
            } else {
                String::new()
            },
            is_correct,
        })
    }

    /// A completion. When `failed`, models a current client: it sets the
    /// retiring `failed` flag *and* `stop_reason = failure` (the signal mgmt
    /// actually reads), with `reason` as the failure detail.
    fn completion(
        id: &str,
        failed: bool,
        reason: Option<&str>,
    ) -> anyhow::Result<SampleCompletion> {
        Ok(SampleCompletion {
            id: crate::validated::NonEmptyTrimmedString::try_new(id)?,
            completion: if failed {
                String::new()
            } else {
                "B".to_string()
            },
            failed,
            failed_reason: reason.map(str::to_string),
            stop_reason: failed.then(|| "failure".to_string()),
            stop_detail: None,
            completion_tokens: None,
        })
    }

    /// Like [`completion`] but with the client-captured `stop_reason` /
    /// `completion_tokens` fields set.
    fn completion_with_stop(
        id: &str,
        stop_reason: &str,
        completion_tokens: i64,
    ) -> anyhow::Result<SampleCompletion> {
        Ok(SampleCompletion {
            id: crate::validated::NonEmptyTrimmedString::try_new(id)?,
            completion: "B".to_string(),
            failed: false,
            failed_reason: None,
            stop_reason: Some(stop_reason.to_string()),
            stop_detail: None,
            completion_tokens: Some(completion_tokens),
        })
    }

    #[test]
    fn build_eval_sample_results_reinjects_failed_metadata_by_id() -> anyhow::Result<()> {
        let completions = vec![
            completion("ok", false, None)?,
            completion("bad", true, Some("server crashed"))?,
        ];
        let scored = vec![scored("ok", true)?, scored("bad", false)?];

        let rows = build_eval_sample_results(&completions, scored)?;

        assert_eq!(rows.len(), 2);
        let ok = rows
            .iter()
            .find(|r| r.id == "ok")
            .context("missing ok row")?;
        let bad = rows
            .iter()
            .find(|r| r.id == "bad")
            .context("missing bad row")?;
        assert!(ok.is_correct);
        assert!(!ok.failed);
        assert_eq!(ok.failed_reason, None);
        assert!(!bad.is_correct);
        assert!(bad.failed);
        assert_eq!(bad.failed_reason.as_deref(), Some("server crashed"));
        // A client-`failed` sample maps to `stop_reason = failure`
        // (source `recorded`); a plain sample with no captured stop_reason
        // stays `None`.
        assert_eq!(ok.stop_reason, None);
        assert_eq!(ok.stop_reason_source, None);
        assert_eq!(ok.stop_detail, None);
        assert_eq!(bad.stop_reason.as_deref(), Some("failure"));
        assert_eq!(bad.stop_reason_source.as_deref(), Some("recorded"));
        // The client sent no `stop_detail`, so it falls back to `failed_reason`.
        assert_eq!(bad.stop_detail.as_deref(), Some("server crashed"));
        Ok(())
    }

    #[test]
    fn build_eval_sample_results_prefers_client_stop_detail_over_failed_reason()
    -> anyhow::Result<()> {
        let mut crashed = completion("bad", true, Some("failed_reason text"))?;
        crashed.stop_detail = Some("stop_detail text".to_string());
        let completions = vec![crashed];
        let scored = vec![scored("bad", false)?];

        let rows = build_eval_sample_results(&completions, scored)?;
        let bad = rows.iter().find(|r| r.id == "bad").context("missing bad")?;
        // Client's own `stop_detail` wins; the fallback only fills a gap.
        assert_eq!(bad.stop_detail.as_deref(), Some("stop_detail text"));
        assert_eq!(bad.failed_reason.as_deref(), Some("failed_reason text"));
        Ok(())
    }

    #[test]
    fn is_failed_sample_keys_on_stop_reason_only() -> anyhow::Result<()> {
        // `stop_reason == failure` counts.
        assert!(is_failed_sample(&completion("a", true, Some("crash"))?));
        // A bare `failed` flag with no `stop_reason` does NOT count — the flag
        // is no longer consulted.
        let mut flag_only = completion("b", false, None)?;
        flag_only.failed = true;
        assert!(!is_failed_sample(&flag_only));
        // A clean sample doesn't count.
        assert!(!is_failed_sample(&completion_with_stop("c", "eos", 3)?));
        Ok(())
    }

    #[test]
    fn build_eval_sample_results_passes_through_client_stop_reason() -> anyhow::Result<()> {
        let completions = vec![
            completion_with_stop("eos", "eos", 42)?,
            completion_with_stop("trunc", "truncated", 8192)?,
        ];
        let scored = vec![scored("eos", true)?, scored("trunc", false)?];

        let rows = build_eval_sample_results(&completions, scored)?;

        let eos = rows
            .iter()
            .find(|r| r.id == "eos")
            .context("missing eos row")?;
        let trunc = rows
            .iter()
            .find(|r| r.id == "trunc")
            .context("missing trunc row")?;
        assert_eq!(eos.stop_reason.as_deref(), Some("eos"));
        assert_eq!(eos.stop_reason_source.as_deref(), Some("recorded"));
        assert_eq!(eos.completion_tokens, Some(42));
        assert_eq!(trunc.stop_reason.as_deref(), Some("truncated"));
        assert_eq!(trunc.completion_tokens, Some(8192));
        Ok(())
    }

    #[test]
    fn build_eval_sample_results_bails_if_scorer_drops_a_failed_row() -> anyhow::Result<()> {
        let completions = vec![
            completion("ok", false, None)?,
            completion("bad", true, Some("crash"))?,
        ];
        // Scorer omits the failed sample's response.
        let scored = vec![scored("ok", true)?];

        let err = build_eval_sample_results(&completions, scored).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("dropped"));
        assert!(msg.contains("bad"));
        Ok(())
    }

    #[test]
    fn build_eval_sample_results_bails_if_scorer_drops_a_non_failed_row() -> anyhow::Result<()> {
        // Concern #1: the verification must catch *any* dropped row, not
        // just failed ones. Without this, a non-failed sample silently
        // missing from the response would skew the accuracy denominator
        // (correct / (n - k)) with no warning.
        let completions = vec![completion("a", false, None)?, completion("b", false, None)?];
        let scored = vec![scored("a", true)?];

        let err = build_eval_sample_results(&completions, scored).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("dropped"));
        assert!(msg.contains("\"b\""));
        Ok(())
    }

    #[test]
    fn build_eval_sample_results_bails_if_scorer_returns_unknown_id() -> anyhow::Result<()> {
        let completions = vec![completion("a", false, None)?];
        // Scorer returned an extra row we didn't request.
        let scored = vec![scored("a", true)?, scored("ghost", false)?];

        let err = build_eval_sample_results(&completions, scored).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown"));
        assert!(msg.contains("ghost"));
        Ok(())
    }

    // ── commit_chunk: per-job isolation ───────────────────────────────────

    use crate::config::{Config as CommitConfig, StorageConfig};
    use crate::stores::{EvalSampleResultStore, JobState, Stores, build_local_fs_stores};
    use crate::types::ClientId;
    use crate::warehouse::MetricRow;
    use async_trait::async_trait;
    use std::sync::Arc;

    fn commit_test_stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = CommitConfig {
            evals_server_url: "http://unused".to_string(),
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..CommitConfig::default()
        };
        build_local_fs_stores(&config)
    }

    /// Eval-results store that always fails `write`, to exercise the per-job
    /// deferral path that triggers on an eval-results failure.
    struct FailingEvalStore;

    #[async_trait]
    impl EvalSampleResultStore for FailingEvalStore {
        async fn write(&self, _job_id: &JobId, _rows: &[EvalSampleResult]) -> anyhow::Result<()> {
            anyhow::bail!("injected eval-results write failure")
        }
        async fn read(&self, _job_id: &JobId) -> anyhow::Result<Option<Vec<EvalSampleResult>>> {
            Ok(None)
        }
        async fn list_job_ids(&self) -> anyhow::Result<Vec<JobId>> {
            Ok(Vec::new())
        }
    }

    fn commit_metric_row(job_id: &str) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: format!("{job_id}-ttft"),
            benchmark_id: BenchmarkId::try_new("bench")?,
            metric: "ttft".to_string(),
            client_id: ClientId::try_new("c1")?,
            device_name: "d".to_string(),
            device_os_name: "linux".to_string(),
            device_chip_model: "chip".to_string(),
            device_ram_bytes: 16_000_000_000,
            model_name: Some("m".to_string()),
            model_quant: Some("q4_0".to_string()),
            value: 34.7,
            unit: "ms".to_string(),
            ..Default::default()
        })
    }

    /// A scored job carrying one warehouse row. `mark_processed` succeeds iff
    /// an incoming body exists for the job, so tests fail a specific job
    /// simply by not writing its incoming body.
    fn commit_scored_job(job_id: &str) -> anyhow::Result<ScoreOutcome> {
        Ok(ScoreOutcome::Scored(ScoredJob {
            job_id: JobId::new_unchecked(job_id),
            benchmark_id: BenchmarkId::try_new("bench")?,
            client_id: ClientId::try_new("c1")?,
            day_key: "2026-06-12".to_string(),
            metric_rows: vec![commit_metric_row(job_id)?],
            eval_sample_results: None,
        }))
    }

    /// One job whose `mark_processed` fails is deferred; the healthy jobs in
    /// the same chunk still commit and leave `incoming/`.
    #[tokio::test]
    async fn commit_chunk_isolates_a_failing_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;

        // good1/good2 have incoming bodies; "poison" does not, so its
        // mark_processed errors while the others succeed.
        for jid in ["good1", "good2"] {
            stores
                .submissions
                .write_incoming(
                    &JobId::new_unchecked(jid),
                    &json!({"message_type": "success"}),
                )
                .await?;
        }

        let outcomes = vec![
            commit_scored_job("good1")?,
            commit_scored_job("poison")?,
            commit_scored_job("good2")?,
        ];
        let deferred = commit_chunk(&stores, &outcomes).await?;

        assert_eq!(deferred, vec![JobId::new_unchecked("poison")]);
        for good in ["good1", "good2"] {
            let rec = stores
                .submissions
                .get_submission(&JobId::new_unchecked(good))
                .await?
                .context("good job should still be findable")?;
            assert!(
                matches!(rec.state, JobState::Processed),
                "{good} not processed"
            );
        }
        // The poison job was never marked, so it isn't in processed/ (and had
        // no incoming body to begin with).
        assert!(
            stores
                .submissions
                .get_submission(&JobId::new_unchecked("poison"))
                .await?
                .is_none()
        );
        Ok(())
    }

    /// A healthy chunk defers nothing and marks every job.
    #[tokio::test]
    async fn commit_chunk_marks_all_when_healthy() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;
        for jid in ["j1", "j2"] {
            stores
                .submissions
                .write_incoming(
                    &JobId::new_unchecked(jid),
                    &json!({"message_type": "success"}),
                )
                .await?;
        }
        let outcomes = vec![commit_scored_job("j1")?, commit_scored_job("j2")?];
        let deferred = commit_chunk(&stores, &outcomes).await?;
        assert!(deferred.is_empty());
        Ok(())
    }

    /// When *every* job fails to commit, that's systemic — `commit_chunk`
    /// errors rather than reporting a quietly empty success.
    #[tokio::test]
    async fn commit_chunk_errors_when_all_jobs_fail() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;
        // No incoming bodies written → every mark_processed fails.
        let outcomes = vec![commit_scored_job("a")?, commit_scored_job("b")?];
        let err = commit_chunk(&stores, &outcomes)
            .await
            .expect_err("all-failed commit should surface an error");
        assert!(err.to_string().contains("commit failed for all"));
        Ok(())
    }

    /// A job whose eval-results write fails is deferred without being marked;
    /// a sibling job that carries no eval results still commits.
    #[tokio::test]
    async fn commit_chunk_defers_job_on_eval_results_failure() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut stores = commit_test_stores(dir.path())?;
        stores.eval_sample_results = Arc::new(FailingEvalStore);

        for jid in ["good", "poison"] {
            stores
                .submissions
                .write_incoming(
                    &JobId::new_unchecked(jid),
                    &json!({"message_type": "success"}),
                )
                .await?;
        }

        // `good` has no eval results, so its commit never touches the failing
        // store; `poison` carries eval results, so its write fails.
        let good = commit_scored_job("good")?;
        let poison = ScoreOutcome::Scored(ScoredJob {
            job_id: JobId::new_unchecked("poison"),
            benchmark_id: BenchmarkId::try_new("bench")?,
            client_id: ClientId::try_new("c1")?,
            day_key: "2026-06-12".to_string(),
            metric_rows: vec![commit_metric_row("poison")?],
            eval_sample_results: Some(Vec::new()),
        });

        let deferred = commit_chunk(&stores, &[good, poison]).await?;

        assert_eq!(deferred, vec![JobId::new_unchecked("poison")]);
        let good_rec = stores
            .submissions
            .get_submission(&JobId::new_unchecked("good"))
            .await?
            .context("good job should still be findable")?;
        assert!(matches!(good_rec.state, JobState::Processed));
        // poison was not marked: its incoming body is still there.
        let poison_rec = stores
            .submissions
            .get_submission(&JobId::new_unchecked("poison"))
            .await?
            .context("poison job should still be findable")?;
        assert!(matches!(poison_rec.state, JobState::Incoming));
        Ok(())
    }

    // ── fast worker: eval routing ─────────────────────────────────────────

    use crate::stores::ScoreQueueStage;
    use std::collections::HashMap as StdHashMap;
    use std::num::NonZeroUsize;

    fn incoming_body(job_id: &str, benchmark_id: &str, benchmark_type: &str) -> Value {
        json!({
            "message_type": "success",
            "benchmark_id": benchmark_id,
            "benchmark_type": benchmark_type,
            "client_id": "c1",
            "job_id": job_id,
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "embedded",
            "device_os_name": "linux",
            "device_os_version": "22.04",
            "device_chip_model": "chip",
            "device_ram_bytes": 16_000_000_000_i64,
            "model_name": "m",
            "model_quant": "q4_0",
            "runtime_name": "rt",
            "runtime_version": "v1",
        })
    }

    /// The inline guard in `read_and_score` routes an eval to to_do — so the
    /// fast scoring loop never makes a /score
    /// call. `evals_server_url` points nowhere; a routed job must not touch it.
    #[tokio::test]
    async fn score_chunk_routes_eval_without_calling_the_service() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = CommitConfig {
            evals_server_url: "http://127.0.0.1:1".to_string(), // would refuse instantly if called
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..CommitConfig::default()
        };
        let stores = build_local_fs_stores(&config)?;

        let mut catalog: StdHashMap<BenchmarkId, Benchmark> = StdHashMap::new();
        catalog.insert(
            BenchmarkId::try_new("eval_test")?,
            Benchmark {
                benchmark_id: BenchmarkId::try_new("eval_test")?,
                def: BenchmarkDef::Eval {
                    parameter_eval_id: "e".to_string(),
                    parameter_dataset_name: "d".to_string(),
                    parameter_max_tokens: 16,
                    parameter_mcq_choices: None,
                },
            },
        );

        let job = JobId::new_unchecked("evaljob");
        stores
            .submissions
            .write_incoming(&job, &incoming_body("evaljob", "eval_test", "eval"))
            .await?;

        // score_chunk must route the eval — not score it — so no connection to
        // the dead URL is made.
        let http_client = reqwest::Client::new();
        let outcome = score_chunk(
            &config,
            &catalog,
            &ModelCatalog::empty(),
            &stores,
            &http_client,
            1,
            std::slice::from_ref(&job),
        )
        .await;

        assert_eq!(outcome.outcomes.len(), 1);
        assert!(matches!(outcome.outcomes[0], ScoreOutcome::Routed { .. }));
        assert!(outcome.newly_failed.is_empty());
        let limit = NonZeroUsize::new(10).ok_or_else(|| anyhow::anyhow!("nonzero"))?;
        assert_eq!(
            stores
                .submissions
                .list_queue(ScoreQueueStage::ToDo, limit)
                .await?,
            vec![job.clone()]
        );
        assert!(stores.submissions.list_incoming(limit).await?.is_empty());
        Ok(())
    }
}
