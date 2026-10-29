use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use anyhow::Context;
use chrono::Utc;
use serde_json::Value;

use pipette_core::benchmark::Benchmark;
use pipette_core::config::Config;
use pipette_core::eval_sample_result::EvalSampleResult;
use pipette_core::warehouse;
use pipette_core::warehouse::MetricRow;
use pipette_plan_types::{
    AsrClientExtras, AsrDecoding, AsrErrorUnit, AsrEvalBenchmark, AsrEvalTask,
    AsrMaxMemoryBenchmark, AsrThroughputBenchmark, BatchSize, BenchmarkDefinition, BenchmarkId,
    BenchmarkType, ClientId, Dataset, DatasetUpstream, DecodeThroughputBenchmark, DeviceFormFactor,
    EndToEndLatencyBenchmark, EvalBenchmark, FailureSource, JobId, MaxMemoryUsageBenchmark,
    ModelFlags, PlanId, PrefillThroughputBenchmark, SampleCompletion, VaultResourceId,
    VlMaxMemoryBenchmark, VlSpecDecodeBenchmark, VlThroughputBenchmark, VlmEvalBenchmark,
    VlmEvalRecipe, VlmPromptProfile,
};
use pipette_store::{JobState, PlanStore, ScoreQueueStage, Stores, SubmissionRecord, VaultStore};

use crate::model_audit::UnresolvedRows;
use crate::model_params::{ModelEntry, ModelParamTable};
use crate::scoring_service;
use crate::scoring_service::{ScoreRequest, ScoreRequestSample, ScoringError};
pub use crate::submission::{FailureSubmission, Submission, SuccessSubmission};
use crate::submission::{
    SuccessInputExt, VlSpecDecodeCounters, answers_every_sample, reports_the_pinned_corpus,
};
use crate::vault_alias::KnownOrigins;
use crate::vault_check::{descriptor_sha256, job_names, ran_source, reported_resources};
use crate::vault_label::{Fill, unlabelled_resources};

pub async fn run_process_submissions(config: &Config, stores: Stores) -> anyhow::Result<()> {
    let chunk_limit = config.score_chunk_size;

    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let model_table = ModelParamTable::load(stores.model_params.as_ref()).await?;
    let models = PassTable::new(&model_table);
    let http_client = crate::http_client::builder_for(&config.evals_server_url)
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(
            config.http_connect_timeout_secs.get(),
        ))
        .build()?;

    let mut routed = 0usize;

    // Listings page past the last id, so jobs that keep failing never hide the
    // ones behind them. Each chunk commits before the next list.
    let mut after: Option<JobId> = None;
    let mut total_failed = 0usize;
    let mut total_scored = 0usize;
    let mut total_failures_recorded = 0usize;
    let mut total_failures_archived = 0usize;
    let mut chunk_idx = 0usize;
    let mut parents = RerunParents::new(&*stores.plans);

    loop {
        let chunk = stores
            .submissions
            .list_incoming_after(after.as_ref(), chunk_limit)
            .await?;
        let Some(last) = chunk.last().cloned() else {
            break;
        };

        chunk_idx += 1;
        let chunk_size = chunk.len();
        println!("Scoring chunk {chunk_idx} ({chunk_size} submission(s), limit {chunk_limit})...");
        tracing::info!(
            chunk = chunk_idx,
            chunk_size,
            chunk_limit = chunk_limit.get(),
            failed_so_far = total_failed,
            "scoring chunk"
        );

        let outcome = score_chunk(
            config,
            &catalog,
            &models,
            &stores,
            &mut parents,
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
                // Deferred: still in `incoming/`, counted as failed.
                ScoreOutcome::Scored(s) if deferred.contains(&s.job_id) => {}
                ScoreOutcome::Scored(_) => total_scored += 1,
                ScoreOutcome::RecordedFailure(s) if deferred.contains(&s.job_id) => {}
                ScoreOutcome::RecordedFailure(_) => total_failures_recorded += 1,
                ScoreOutcome::ArchivedLockedFailure(s) if deferred.contains(&s.job_id) => {}
                ScoreOutcome::ArchivedLockedFailure(_) => total_failures_archived += 1,
                ScoreOutcome::Routed { .. } => routed += 1,
            }
        }
        total_failed += deferred.len() + outcome.newly_failed.len();

        // A short listing means the store is exhausted.
        if chunk_size < chunk_limit.get() {
            break;
        }
        after = Some(last);
    }

    let finalized = finalize_scored_evals(
        config,
        &catalog,
        &models,
        &http_client,
        &stores,
        &mut parents,
        chunk_limit,
    )
    .await?;

    let total_seen =
        total_scored + total_failures_recorded + total_failures_archived + total_failed;
    println!(
        "Done. {routed} eval(s) routed to scoring; {total_seen} non-eval submission(s) across \
         {chunk_idx} chunk(s): {total_scored} scored, {total_failures_recorded} failure(s) \
         recorded, {total_failures_archived} failure(s) archived past the warehouse lock window, \
         {total_failed} failed; {finalized} eval(s) finalized."
    );
    tracing::info!(
        routed,
        scored = total_scored,
        failures_recorded = total_failures_recorded,
        failures_archived = total_failures_archived,
        failed = total_failed,
        chunks = chunk_idx,
        finalized,
        "process-submissions: done"
    );

    // Loud enough for cron monitoring to catch a lock window that is too narrow.
    if total_failures_archived > 0 {
        tracing::warn!(
            archived = total_failures_archived,
            lock_age_secs = config.warehouse_lock_age_secs.get(),
            "failure submissions archived unrecorded: their day partitions are past the \
             warehouse lock window"
        );
    }

    if total_failed > 0 {
        tracing::warn!(
            failed = total_failed,
            "some submissions failed to score, will retry on next run"
        );
    }

    // Last, so it reads as a note on the catalog, not a fault in one chunk.
    models.report_unresolved();
    Ok(())
}

/// Evals are the only type that needs the slow scoring-service call.
fn submission_is_eval(catalog: &HashMap<BenchmarkId, Benchmark>, body: &serde_json::Value) -> bool {
    match crate::submission::parse_stored_submission(body) {
        Ok(Submission::Success(s)) => catalog.get(&s.wire.benchmark_id).is_some_and(|b| {
            matches!(
                b.def,
                BenchmarkDefinition::Eval(_)
                    | BenchmarkDefinition::VlmEval(_)
                    | BenchmarkDefinition::AsrEval(_)
            )
        }),
        _ => false,
    }
}

/// Drain `score-queue/to_finalize/`; one job's failure defers only that job.
async fn finalize_scored_evals(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    models: &PassTable<'_>,
    http_client: &reqwest::Client,
    stores: &Stores,
    parents: &mut RerunParents<'_>,
    chunk_limit: std::num::NonZeroUsize,
) -> anyhow::Result<usize> {
    let mut after: Option<JobId> = None;
    let mut finalized = 0usize;
    loop {
        let chunk = stores
            .submissions
            .list_queue_after(ScoreQueueStage::ToFinalize, after.as_ref(), chunk_limit)
            .await?;
        let Some(last) = chunk.last().cloned() else {
            break;
        };
        let chunk_size = chunk.len();
        for job_id in chunk {
            let finalize = finalize_one(
                config,
                catalog,
                models,
                http_client,
                stores,
                parents,
                &job_id,
            );
            match finalize.await {
                Ok(()) => {
                    finalized += 1;
                    clear_failed_attempts(stores, &job_id).await;
                }
                Err(e) => {
                    tracing::warn!(job_id = %job_id, error = %e, "finalize failed; deferring");
                    count_failed_attempt(stores, ScoreQueueStage::ToFinalize, &job_id, &e).await;
                }
            }
        }
        if chunk_size < chunk_limit.get() {
            break;
        }
        after = Some(last);
    }
    Ok(finalized)
}

/// Archive before dequeue, so a crash re-finalizes rather than dropping the
/// processed copy; every write is idempotent.
async fn finalize_one(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    models: &PassTable<'_>,
    http_client: &reqwest::Client,
    stores: &Stores,
    parents: &mut RerunParents<'_>,
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
    let names = VaultNames::read(
        stores,
        &success.job_id,
        success.rescored_from.as_ref(),
        success.wire.model_descriptor.as_deref(),
        success.wire.runtime_descriptor.as_deref(),
    )
    .await?;
    let rerun_of = parents.of(&success.job_id).await?;
    let scored = score_success(
        config,
        catalog,
        models,
        http_client,
        *success,
        Some(score),
        &names,
    )
    .await?
    .rerunning(rerun_of);

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
    tracing::info!(
        job_id = %job_id,
        benchmark_id = %scored.benchmark_id,
        client_id = %scored.client_id,
        "finalized eval submission"
    );
    Ok(())
}

/// A queued eval is parked after failing this many passes, about an hour on
/// the five-minute crons: long enough to ride out a scorer incident.
pub const MAX_SCORE_ATTEMPTS: u64 = 12;

/// Slow eval worker: score `to_do/` and stage the result into `to_finalize/`.
/// Takes no mutate lock (it never touches the warehouse), so a multi-minute
/// eval never blocks the fast worker.
pub async fn run_score_eval(config: &Config, stores: Stores) -> anyhow::Result<()> {
    let chunk_limit = config.score_chunk_size;
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let http_client = crate::http_client::builder_for(&config.evals_server_url)
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(
            config.http_connect_timeout_secs.get(),
        ))
        .build()?;

    let mut failed = 0usize;
    let mut parked = 0usize;
    let mut scored = 0usize;
    let mut service_down = false;
    let mut after: Option<JobId> = None;
    'outer: loop {
        let chunk = stores
            .submissions
            .list_queue_after(ScoreQueueStage::ToDo, after.as_ref(), chunk_limit)
            .await?;
        let Some(last) = chunk.last().cloned() else {
            break;
        };
        let chunk_size = chunk.len();
        for job_id in chunk {
            match score_eval_one(config, &catalog, &stores, &http_client, &job_id).await {
                Ok(()) => {
                    scored += 1;
                    clear_failed_attempts(&stores, &job_id).await;
                }
                Err(e) if is_service_unreachable(&e) => {
                    tracing::warn!(job_id = %job_id, error = %e, "scoring service unreachable; pausing eval worker");
                    // Counted, or a job that always times out is retried
                    // forever ahead of every job after it.
                    if is_service_timeout(&e) {
                        parked += usize::from(
                            count_failed_attempt(&stores, ScoreQueueStage::ToDo, &job_id, &e).await,
                        );
                    }
                    service_down = true;
                    break 'outer;
                }
                Err(e) => {
                    tracing::error!(job_id = %job_id, error = %e, "failed to score eval submission");
                    failed += 1;
                    parked += usize::from(
                        count_failed_attempt(&stores, ScoreQueueStage::ToDo, &job_id, &e).await,
                    );
                }
            }
        }
        if chunk_size < chunk_limit.get() {
            break;
        }
        after = Some(last);
    }
    let suffix = if service_down {
        " Paused: scoring service unreachable; remaining to_do jobs retried next invocation."
    } else {
        ""
    };
    println!("Eval scoring done: {scored} scored, {failed} failed, {parked} parked.{suffix}");
    tracing::info!(
        scored,
        failed,
        parked,
        paused = service_down,
        "score-eval: done"
    );
    Ok(())
}

async fn clear_failed_attempts(stores: &Stores, job_id: &JobId) {
    if let Err(e) = stores
        .submissions
        .dequeue(ScoreQueueStage::Failures, job_id)
        .await
    {
        tracing::warn!(job_id = %job_id, error = %e, "failed to clear scoring failures");
    }
}

/// [`record_failed_attempt`], logged rather than failing the pass.
async fn count_failed_attempt(
    stores: &Stores,
    stage: ScoreQueueStage,
    job_id: &JobId,
    error: &anyhow::Error,
) -> bool {
    record_failed_attempt(stores, stage, job_id, error)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(job_id = %job_id, error = %e, "failed to record a scoring failure");
            false
        })
}

/// Count a failed pass in `failures/`; at [`MAX_SCORE_ATTEMPTS`] move the job
/// from `stage` (`to_do/` or `to_finalize/`) to `parked/`, where no pass
/// retries it. The `failures/` entry stays beside it with the last error. True
/// when this call parked it.
async fn record_failed_attempt(
    stores: &Stores,
    stage: ScoreQueueStage,
    job_id: &JobId,
    error: &anyhow::Error,
) -> anyhow::Result<bool> {
    let submissions = &stores.submissions;
    let attempts = submissions
        .read_queue(ScoreQueueStage::Failures, job_id)
        .await?
        .and_then(|f| f.get("attempts").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
        + 1;
    let error = format!("{error:#}");
    submissions
        .enqueue(
            ScoreQueueStage::Failures,
            job_id,
            &serde_json::json!({ "attempts": attempts, "error": error }),
        )
        .await?;
    if attempts < MAX_SCORE_ATTEMPTS {
        return Ok(false);
    }
    let Some(entry) = submissions.read_queue(stage, job_id).await? else {
        return Ok(false);
    };
    // `parked/` holds the raw submission, as `to_do/` does; a retry rescores it.
    let body = match stage {
        ScoreQueueStage::ToFinalize => entry
            .get("submission")
            .cloned()
            .context("to_finalize payload missing `submission`")?,
        ScoreQueueStage::ToDo | ScoreQueueStage::Failures | ScoreQueueStage::Parked => entry,
    };
    submissions
        .enqueue(ScoreQueueStage::Parked, job_id, &body)
        .await?;
    submissions.dequeue(stage, job_id).await?;
    tracing::warn!(
        job_id = %job_id,
        stage = stage.leaf(),
        benchmark_id = body.get("benchmark_id").and_then(serde_json::Value::as_str),
        client_id = body.get("client_id").and_then(serde_json::Value::as_str),
        attempts,
        error,
        "eval failed every scoring pass; parked in score-queue/parked"
    );
    Ok(true)
}

/// Idempotent: an already staged job drops its `to_do` entry without
/// re-paying the multi-minute service call.
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
        tracing::info!(
            job_id = %job_id,
            "eval already staged for finalize; dropped its to_do entry"
        );
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
    let (eval_id, dataset_name, enable_thinking, prompt_profile, samples_digest) =
        match &benchmark.def {
            BenchmarkDefinition::Eval(EvalBenchmark {
                parameter_eval_id,
                parameter_dataset_name,
                ..
            }) => (
                parameter_eval_id.as_str(),
                parameter_dataset_name.as_str(),
                requested_enable_thinking(&success),
                None,
                None,
            ),
            BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
                parameter_lmms_task,
                parameter_dataset_version,
                parameter_prompt_profile,
                dataset,
                recipe,
                ..
            }) => (
                parameter_lmms_task.as_str(),
                parameter_dataset_version.as_str(),
                vlm_eval_thinking(&success, recipe.as_ref()),
                parameter_prompt_profile
                    .as_ref()
                    .map(VlmPromptProfile::as_str),
                dataset
                    .as_ref()
                    .map(|dataset| ran_the_pinned_corpus(&success, dataset))
                    .transpose()?,
            ),
            // Thinking is the recipe's, always off: ingestion refuses a cell
            // that states another mode.
            BenchmarkDefinition::AsrEval(asr) => (
                asr.parameter_asr_task.as_str(),
                asr.parameter_dataset_version.as_str(),
                asr_eval_thinking(asr),
                Some(asr.parameter_prompt_profile.as_str()),
                Some(asr_ran_the_pinned_corpus(&success, asr)?),
            ),
            BenchmarkDefinition::PrefillThroughput(_)
            | BenchmarkDefinition::DecodeThroughput(_)
            | BenchmarkDefinition::EndToEndLatency(_)
            | BenchmarkDefinition::MaxMemoryUsage(_)
            | BenchmarkDefinition::VlThroughput(_)
            | BenchmarkDefinition::VlMaxMemory(_)
            | BenchmarkDefinition::AsrThroughput(_)
            | BenchmarkDefinition::AsrMaxMemory(_)
            | BenchmarkDefinition::VlSpecDecode(_) => {
                anyhow::bail!("to_do job {job_id} is not an eval benchmark")
            }
        };

    answered_every_sample(&benchmark.def, &success)?;
    let completions = eval_completions(&success)?;
    let score = call_score_service(
        http_client,
        config,
        eval_id,
        dataset_name,
        &completions,
        enable_thinking,
        prompt_profile,
        samples_digest,
    )
    .await?;
    let payload = serde_json::json!({ "submission": body, "score": score });
    stores
        .submissions
        .enqueue(ScoreQueueStage::ToFinalize, job_id, &payload)
        .await?;
    stores
        .submissions
        .dequeue(ScoreQueueStage::ToDo, job_id)
        .await?;
    tracing::info!(job_id = %job_id, "staged scored eval for finalize");
    Ok(())
}

struct ChunkOutcome {
    outcomes: Vec<ScoreOutcome>,
    newly_failed: Vec<JobId>,
}

/// The downcast relies on nothing between here and `scoring_service::score`
/// re-wrapping the `ScoringError`.
fn is_service_unreachable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ScoringError>()
        .is_some_and(ScoringError::is_unreachable)
}

fn is_service_timeout(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ScoringError>()
        .is_some_and(ScoringError::is_timeout)
}

/// Score every submission in `chunk`; a per-job error never aborts the run.
#[expect(
    clippy::too_many_arguments,
    reason = "the pass's catalogs, stores and re-run cache, which every chunk shares"
)]
async fn score_chunk(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    models: &PassTable<'_>,
    stores: &Stores,
    parents: &mut RerunParents<'_>,
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

        let scoring = read_and_score(
            config,
            catalog,
            models,
            stores,
            parents,
            http_client,
            job_id,
        );
        match scoring.await {
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
            Ok(ScoreOutcome::RecordedFailure(recorded)) => {
                let rows = recorded.metric_rows.len();
                println!("{prefix} Recorded failure for job {job_id} ({rows} row(s)).");
                outcome
                    .outcomes
                    .push(ScoreOutcome::RecordedFailure(recorded));
            }
            Ok(ScoreOutcome::ArchivedLockedFailure(archived)) => {
                println!(
                    "{prefix} Archived failure for job {job_id} unrecorded: partition day={} is \
                     past the warehouse lock window.",
                    archived.day_key
                );
                outcome
                    .outcomes
                    .push(ScoreOutcome::ArchivedLockedFailure(archived));
            }
            Ok(ScoreOutcome::Routed { job_id: jid }) => {
                println!("{prefix} Routed eval job {job_id} to score-queue/to_do.");
                tracing::info!(job_id = %job_id, "routed eval submission to score-queue/to_do");
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

async fn read_and_score(
    config: &Config,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    models: &PassTable<'_>,
    stores: &Stores,
    parents: &mut RerunParents<'_>,
    http_client: &reqwest::Client,
    job_id: &JobId,
) -> anyhow::Result<ScoreOutcome> {
    let record = stores
        .submissions
        .get_submission(job_id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read submission: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("submission not found"))?;

    // Never score an eval here: the multi-minute /score call would run inside
    // the mutate lock.
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

    score_submission(
        config,
        catalog,
        models,
        stores,
        parents,
        http_client,
        &record,
    )
    .await
}

/// A failure body becomes a [`ScoredJob`] too, so both kinds share the commit
/// path and its at-least-once guarantee.
enum ScoreOutcome {
    Scored(ScoredJob),
    RecordedFailure(ScoredJob),
    /// A failure whose day partition is closed to writes: no rows, archived
    /// unrecorded.
    ArchivedLockedFailure(ScoredJob),
    /// An eval moved to `score-queue/to_do/`; it already left `incoming/`.
    Routed {
        job_id: JobId,
    },
}

/// Persist a chunk's outcomes, returning the jobs deferred to the next run.
///
/// A warehouse write failure is infrastructure and aborts the run. The rest
/// commits per job, `mark_processed` last, so one poison job cannot block the
/// backlog; every job failing is systemic and an error.
async fn commit_chunk(stores: &Stores, outcomes: &[ScoreOutcome]) -> anyhow::Result<Vec<JobId>> {
    let scored: Vec<&ScoredJob> = outcomes
        .iter()
        .filter_map(|o| match o {
            ScoreOutcome::Scored(s)
            | ScoreOutcome::RecordedFailure(s)
            | ScoreOutcome::ArchivedLockedFailure(s) => Some(s),
            ScoreOutcome::Routed { .. } => None,
        })
        .collect();

    // By variant, not by empty `metric_rows`: the S3 store rejects an empty row
    // set, which would abort the chunk on the partition the archive leaves alone.
    // A recorded failure with no rows (its type intends no metric) is deferred
    // alone, loudly: its empty partition would abort the whole chunk too, and
    // marking it processed would lose its row.
    let mut rowless: Vec<&ScoredJob> = Vec::new();
    let mut with_rows: Vec<&ScoredJob> = Vec::new();
    for outcome in outcomes {
        match outcome {
            ScoreOutcome::RecordedFailure(s) if s.metric_rows.is_empty() => rowless.push(s),
            ScoreOutcome::Scored(s) | ScoreOutcome::RecordedFailure(s) => with_rows.push(s),
            ScoreOutcome::ArchivedLockedFailure(_) | ScoreOutcome::Routed { .. } => {}
        }
    }
    for job in &rowless {
        tracing::error!(
            job_id = %job.job_id,
            benchmark_id = %job.benchmark_id,
            "a recorded failure has no warehouse rows; deferring it to the next run"
        );
    }
    let partitions = with_rows.into_iter().fold(
        BTreeMap::<(BenchmarkId, ClientId, String), Vec<MetricRow>>::new(),
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
    let committing: Vec<&ScoredJob> = scored
        .iter()
        .copied()
        .filter(|job| !rowless.iter().any(|r| r.job_id == job.job_id))
        .collect();
    for job in &committing {
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

    if !committing.is_empty() && deferred.len() == committing.len() {
        anyhow::bail!(
            "commit failed for all {} scored job(s) in chunk; submissions store likely unavailable",
            committing.len()
        );
    }

    deferred.extend(rowless.iter().map(|job| job.job_id.clone()));
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

impl ScoredJob {
    /// Name the job this one runs again on every row.
    fn rerunning(mut self, rerun_of: Option<JobId>) -> Self {
        let rerun_of = rerun_of.map(|parent| parent.to_string());
        self.metric_rows
            .iter_mut()
            .for_each(|row| row.rerun_of.clone_from(&rerun_of));
        self
    }
}

/// The job each re-run job runs again, read off the manifests that link it.
///
/// A read error defers the job: a warehouse row is written once, so a NULL
/// link would stick.
struct RerunParents<'a> {
    plans: &'a dyn PlanStore,
    by_plan: HashMap<PlanId, BTreeMap<JobId, JobId>>,
}

impl<'a> RerunParents<'a> {
    fn new(plans: &'a dyn PlanStore) -> Self {
        Self {
            plans,
            by_plan: HashMap::new(),
        }
    }

    /// Every plan that holds a re-run job names the same parent, so the first
    /// that names one answers.
    async fn of(&mut self, job_id: &JobId) -> anyhow::Result<Option<JobId>> {
        let plan_ids = self
            .plans
            .list_job_plan_links(job_id)
            .await
            .with_context(|| format!("listing the plans that link job {job_id}"))?;
        for plan_id in plan_ids {
            let rerun_of = match self.by_plan.entry(plan_id) {
                Entry::Occupied(known) => known.into_mut(),
                Entry::Vacant(slot) => {
                    let manifest = self
                        .plans
                        .get_plan(slot.key())
                        .await
                        .with_context(|| format!("reading plan {}", slot.key()))?;
                    slot.insert(manifest.map(|m| m.rerun_of).unwrap_or_default())
                }
            };
            if let Some(parent) = rerun_of.get(job_id) {
                return Ok(Some(parent.clone()));
            }
        }
        Ok(None)
    }
}

async fn score_submission(
    config: &Config,
    catalog: &HashMap<BenchmarkId, pipette_core::benchmark::Benchmark>,
    models: &PassTable<'_>,
    stores: &Stores,
    parents: &mut RerunParents<'_>,
    http_client: &reqwest::Client,
    submission_record: &SubmissionRecord,
) -> anyhow::Result<ScoreOutcome> {
    if submission_record.state != JobState::Incoming {
        anyhow::bail!("submission {} is not incoming", submission_record.job_id);
    }

    // Tolerates the ~20k stored bodies that lack `message_type`.
    let parsed = crate::submission::parse_stored_submission(&submission_record.body)
        .map_err(|e| anyhow::anyhow!("invalid submission body: {e}"))?;

    match parsed {
        Submission::Failure(f) => {
            // An old body targets a closed partition, whose refused write would
            // abort every chunk forever, so it is archived instead. The archive
            // does no catalog lookup, so a body whose benchmark is gone drains
            // too.
            // `Config::validate` caps this well inside `i64`.
            let lock_age =
                chrono::TimeDelta::seconds(i64::try_from(config.warehouse_lock_age_secs.get())?);
            if warehouse::day_partition_is_open(f.submitted_at, Utc::now(), lock_age) {
                let names = VaultNames::read(
                    stores,
                    &f.job_id,
                    None,
                    f.wire.model_descriptor.as_deref(),
                    f.wire.runtime_descriptor.as_deref(),
                )
                .await?;
                let rerun_of = parents.of(&f.job_id).await?;
                record_failure(catalog, models, *f, &names)
                    .map(|recorded| ScoreOutcome::RecordedFailure(recorded.rerunning(rerun_of)))
            } else {
                archive_locked_failure(*f).map(ScoreOutcome::ArchivedLockedFailure)
            }
        }
        Submission::Success(success) => {
            let names = VaultNames::read(
                stores,
                &success.job_id,
                success.rescored_from.as_ref(),
                success.wire.model_descriptor.as_deref(),
                success.wire.runtime_descriptor.as_deref(),
            )
            .await?;
            let rerun_of = parents.of(&success.job_id).await?;
            score_success(config, catalog, models, http_client, *success, None, &names)
                .await
                .map(|scored| ScoreOutcome::Scored(scored.rerunning(rerun_of)))
        }
    }
}

/// The configuration strings and content ids on every metric row of one
/// submission. Failure and scored rows share this type so they group alike.
struct CanonicalConfig {
    model_descriptor: Option<String>,
    model_vault_descriptor: Option<String>,
    runtime_descriptor: Option<String>,
    model_descriptor_sha256: Option<String>,
    runtime_descriptor_sha256: Option<String>,
    model_flags: Option<String>,
    runtime_flags: Option<String>,
    model_flags_sha256: Option<String>,
    runtime_flags_sha256: Option<String>,
    benchmark_flags: Option<String>,
    benchmark_flags_sha256: Option<String>,
    requested_model_flags: Option<String>,
    requested_runtime_flags: Option<String>,
    requested_benchmark_flags: Option<String>,
    requested_model_flags_sha256: Option<String>,
    requested_runtime_flags_sha256: Option<String>,
    requested_benchmark_flags_sha256: Option<String>,
    client_configuration: Option<String>,
    client_extras: Option<String>,
}

/// What a submission carries before canonicalization. Named fields, so a call
/// site cannot swap two optional strings silently.
struct RawConfig<'a> {
    /// The public alias of a vault model, or else the reported descriptor.
    model_descriptor: Option<&'a str>,
    /// The reported descriptor, when [`Self::model_descriptor`] is an alias.
    model_vault_descriptor: Option<&'a str>,
    runtime_descriptor: Option<&'a str>,
    model_flags: Option<&'a str>,
    runtime_flags: Option<&'a str>,
    benchmark_flags: Option<&'a str>,
    /// Already canonical on the stored record, so the scorer only hashes them.
    requested_model_flags: Option<String>,
    requested_runtime_flags: Option<String>,
    requested_benchmark_flags: Option<String>,
    client_configuration: Option<&'a str>,
    client_extras: Option<&'a str>,
}

impl CanonicalConfig {
    /// Canonicalize each string and hash the canonical form. A plain-string flag
    /// (`--n-gpu-layers 999`) passes through trimmed. Descriptors first take the
    /// vault `custom_label` from `labels`, as `fix-vault-labels` does.
    fn new(raw: RawConfig<'_>, labels: &Fill) -> Self {
        let hash = |s: &Option<String>| s.as_deref().map(pipette_core::canonical_json::sha256_hex);
        // A filled descriptor comes back canonical.
        let descriptor = |reported: &str| {
            let (filled, _) = labels.value(reported);
            filled.unwrap_or_else(|| pipette_core::canonical_json::canonicalize_str(reported))
        };

        let model_descriptor = raw.model_descriptor.map(descriptor);
        // Behind an alias every vault source is an import, which takes no label.
        let model_vault_descriptor = raw
            .model_vault_descriptor
            .map(pipette_core::canonical_json::canonicalize_str);
        let runtime_descriptor = raw.runtime_descriptor.map(descriptor);
        let model_flags = pipette_core::canonical_json::canonicalize_flags(raw.model_flags);
        let runtime_flags = pipette_core::canonical_json::canonicalize_flags(raw.runtime_flags);
        let benchmark_flags = pipette_core::canonical_json::canonicalize_flags(raw.benchmark_flags);

        Self {
            model_descriptor_sha256: model_descriptor.as_deref().map(descriptor_sha256),
            runtime_descriptor_sha256: runtime_descriptor.as_deref().map(descriptor_sha256),
            model_flags_sha256: hash(&model_flags),
            runtime_flags_sha256: hash(&runtime_flags),
            benchmark_flags_sha256: hash(&benchmark_flags),
            requested_model_flags_sha256: hash(&raw.requested_model_flags),
            requested_runtime_flags_sha256: hash(&raw.requested_runtime_flags),
            requested_benchmark_flags_sha256: hash(&raw.requested_benchmark_flags),
            model_descriptor,
            model_vault_descriptor,
            runtime_descriptor,
            model_flags,
            runtime_flags,
            benchmark_flags,
            requested_model_flags: raw.requested_model_flags,
            requested_runtime_flags: raw.requested_runtime_flags,
            requested_benchmark_flags: raw.requested_benchmark_flags,
            client_configuration: raw.client_configuration.map(str::to_owned),
            client_extras: raw.client_extras.map(str::to_owned),
        }
    }
}

async fn score_success(
    config: &Config,
    catalog: &HashMap<BenchmarkId, pipette_core::benchmark::Benchmark>,
    models: &PassTable<'_>,
    http_client: &reqwest::Client,
    submission: SuccessSubmission,
    eval_response: Option<scoring_service::ScoreResponse>,
    names: &VaultNames,
) -> anyhow::Result<ScoredJob> {
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
    let (benchmark_params, benchmark_params_sha256) = benchmark_params_columns(benchmark)?;

    tracing::debug!(
        job_id = %submission.job_id,
        benchmark_id = %benchmark_id,
        client_id = %client_id,
        benchmark_type = %benchmark.benchmark_type(),
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

    let (model_descriptor, model_vault_descriptor) = recorded_model(
        submission.wire.model_descriptor.as_deref(),
        names.alias.as_deref(),
    );
    let (model_params_total_millions, model_params_active_millions) = resolve_mill_params(
        models,
        metrics.len(),
        &submission,
        model_descriptor,
        model_vault_descriptor,
    );

    let day_key = warehouse::day_key_from_timestamp(submission.submitted_at.timestamp_micros())?;
    let benchmark_type = benchmark.benchmark_type();
    let submitted_at_us = submission.submitted_at.timestamp_micros();
    // A copy carries the time of the device run it scores; any other
    // submission is that run.
    let run_submitted_at_us = submission
        .run_submitted_at
        .or_else(|| {
            submission
                .rescored_from
                .is_none()
                .then_some(submission.submitted_at)
        })
        .map(|at| at.timestamp_micros());
    let scored_at_us = scored_at.timestamp_micros();

    // VL and ASR throughput record their token counts as observation columns on
    // every metric row, not as metrics, each type under its own pair.
    let ((obs_prefill_tokens, obs_image_tokens), (obs_asr_prompt_tokens, obs_asr_audio_tokens)) =
        match benchmark.def {
            BenchmarkDefinition::VlThroughput(_) => (
                (
                    submission.wire.prompt_tokens.map(i32::from),
                    submission.wire.image_tokens.map(i32::from),
                ),
                (None, None),
            ),
            BenchmarkDefinition::AsrThroughput(_) => (
                (None, None),
                (
                    submission.wire.asr_prompt_tokens.map(i32::from),
                    submission.wire.asr_audio_tokens.map(i32::from),
                ),
            ),
            BenchmarkDefinition::PrefillThroughput(_)
            | BenchmarkDefinition::DecodeThroughput(_)
            | BenchmarkDefinition::EndToEndLatency(_)
            | BenchmarkDefinition::MaxMemoryUsage(_)
            | BenchmarkDefinition::Eval(_)
            | BenchmarkDefinition::VlMaxMemory(_)
            | BenchmarkDefinition::AsrMaxMemory(_)
            | BenchmarkDefinition::VlmEval(_)
            | BenchmarkDefinition::AsrEval(_)
            | BenchmarkDefinition::VlSpecDecode(_) => ((None, None), (None, None)),
        };

    // An ASR row's client_extras keys, validated at intake, stored in their wire
    // spelling.
    let (obs_asr_context_sizing, obs_asr_audio_tokens_source, obs_asr_beyond_limit) =
        match benchmark.def {
            BenchmarkDefinition::AsrThroughput(_) | BenchmarkDefinition::AsrMaxMemory(_) => {
                let extras = AsrClientExtras::parse(submission.wire.client_extras.as_deref())?;
                (
                    extras.context_sizing.map(|s| s.to_string()),
                    extras.audio_tokens_source.map(|s| s.to_string()),
                    extras.beyond_model_audio_limit,
                )
            }
            BenchmarkDefinition::PrefillThroughput(_)
            | BenchmarkDefinition::DecodeThroughput(_)
            | BenchmarkDefinition::EndToEndLatency(_)
            | BenchmarkDefinition::MaxMemoryUsage(_)
            | BenchmarkDefinition::Eval(_)
            | BenchmarkDefinition::VlThroughput(_)
            | BenchmarkDefinition::VlMaxMemory(_)
            | BenchmarkDefinition::VlmEval(_)
            | BenchmarkDefinition::AsrEval(_)
            | BenchmarkDefinition::VlSpecDecode(_) => (None, None, None),
        };

    // Compared, recorded, never enforced: a corpus that moved upstream is a fact
    // about the run, and the row must say which corpus it measured.
    let dataset_digest = submission.wire.observation_dataset_digest.clone();
    let dataset_digest_matched = dataset_digest.as_ref().and_then(|reported| {
        benchmark
            .dataset()
            .map(|dataset| dataset.digest.as_ref() == reported.as_ref())
    });
    if dataset_digest_matched == Some(false) {
        tracing::warn!(
            job_id = %submission.job_id,
            benchmark_id = %benchmark_id,
            reported = %dataset_digest.as_ref().map_or("", |d| d.as_ref()),
            expected = %benchmark.dataset().map_or("", |d| d.digest.as_ref()),
            "the device measured a different corpus than the catalog pins"
        );
    }
    // Gated on the type: a stray field on another type must stay out of a
    // column whose contract says null.
    let spec_decode = matches!(benchmark.def, BenchmarkDefinition::VlSpecDecode(_));
    let obs_turns = spec_decode
        .then_some(submission.wire.vl_spec_decode_turns)
        .flatten()
        .map(i32::from);
    let output_digest = spec_decode
        .then_some(submission.wire.observation_output_digest.as_ref())
        .flatten()
        .map(|d| d.as_ref().to_string());
    let speculative_enabled = resolved_speculation(
        &benchmark.def,
        submission.wire.runtime_flags.as_ref().map(AsRef::as_ref),
    );

    let raw = RawConfig {
        model_descriptor,
        model_vault_descriptor,
        runtime_descriptor: submission.wire.runtime_descriptor.as_deref(),
        model_flags: submission.wire.model_flags.as_ref().map(AsRef::as_ref),
        runtime_flags: submission.wire.runtime_flags.as_ref().map(AsRef::as_ref),
        benchmark_flags: submission.wire.benchmark_flags.as_deref(),
        requested_model_flags: submission.requested_model_flags.clone(),
        requested_runtime_flags: submission.requested_runtime_flags.clone(),
        requested_benchmark_flags: submission.requested_benchmark_flags.clone(),
        client_configuration: submission.wire.client_configuration.as_deref(),
        client_extras: submission.wire.client_extras.as_deref(),
    };
    let cfg = CanonicalConfig::new(raw, &names.labels);

    let parquet_rows: Vec<MetricRow> = metrics
        .iter()
        .enumerate()
        .map(|(i, metric)| MetricRow {
            result_id: format!("{}_{i}", submission.job_id),
            job_id: Some(submission.job_id.to_string()),
            benchmark_id: benchmark_id.clone(),
            benchmark_type,
            metric: metric.metric.clone(),
            client_id: client_id.clone(),
            device_name: Some(submission.wire.device_name.clone().into()),
            device_form_factor: Some(device_form_factor),
            device_os_name: Some(submission.wire.device_os_name.clone().into()),
            device_os_version: Some(submission.wire.device_os_version.clone().into()),
            device_os_build: submission.wire.device_os_build.clone().map(Into::into),
            device_os_security_patch: submission
                .wire
                .device_os_security_patch
                .clone()
                .map(Into::into),
            device_cpu_arch: submission.wire.device_cpu_arch.clone().map(Into::into),
            device_os_family: submission.wire.device_os_family.clone().map(Into::into),
            device_chip_model: Some(submission.wire.device_chip_model.clone().into()),
            device_gpu_model: submission.wire.device_gpu_model.clone().map(Into::into),
            device_gpu_vram_bytes: submission.wire.device_gpu_vram_bytes.map(i64::from),
            device_npu_model: submission.wire.device_npu_model.clone().map(Into::into),
            device_npu_vram_bytes: submission.wire.device_npu_vram_bytes.map(i64::from),
            device_ram_bytes: Some(submission.wire.device_ram_bytes.into()),
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
            device_hardware_readings: submission.wire.device_hardware_readings.clone(),
            device_os_throttling_readings: submission.wire.device_os_throttling_readings.clone(),
            readiness_gate_waits: submission.wire.readiness_gate_waits.clone(),
            model_name: submission.wire.model_name.clone().map(Into::into),
            model_quant: submission.wire.model_quant.clone().map(Into::into),
            model_params_total_millions,
            model_params_active_millions,
            model_flags: cfg.model_flags.clone(),
            model_flags_sha256: cfg.model_flags_sha256.clone(),
            runtime_name: submission.wire.runtime_name.clone().map(Into::into),
            runtime_version: submission.wire.runtime_version.clone().map(Into::into),
            runtime_flags: cfg.runtime_flags.clone(),
            runtime_flags_sha256: cfg.runtime_flags_sha256.clone(),
            runtime_cpu_variant: submission.wire.runtime_cpu_variant.clone().map(Into::into),
            client_version: submission.wire.client_version.clone().map(Into::into),
            value: Some(metric.value),
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
            parameter_audio_duration_ms: get_param_audio_duration_ms(benchmark),
            parameter_audio_sample_rate_hz: get_param_audio_sample_rate_hz(benchmark),
            parameter_audio_channels: get_param_audio_channels(benchmark),
            observation_vl_throughput_prefill_tokens: obs_prefill_tokens,
            observation_vl_throughput_image_tokens: obs_image_tokens,
            observation_asr_throughput_prefill_tokens: obs_asr_prompt_tokens,
            observation_asr_throughput_audio_tokens: obs_asr_audio_tokens,
            observation_asr_context_sizing: obs_asr_context_sizing.clone(),
            observation_asr_audio_tokens_source: obs_asr_audio_tokens_source.clone(),
            observation_asr_beyond_model_audio_limit: obs_asr_beyond_limit,
            // No per-type gate: accepted on every benchmark type.
            observation_max_swap_bytes: submission.wire.observation_max_swap_bytes.map(i64::from),
            observation_max_host_bytes: submission.wire.observation_max_host_bytes.map(i64::from),
            score_runtime_version: derived.score_runtime_version.clone(),
            eval_metadata: derived.eval_metadata.clone(),
            model_descriptor: cfg.model_descriptor.clone(),
            runtime_descriptor: cfg.runtime_descriptor.clone(),
            model_descriptor_sha256: cfg.model_descriptor_sha256.clone(),
            model_vault_descriptor: cfg.model_vault_descriptor.clone(),
            runtime_descriptor_sha256: cfg.runtime_descriptor_sha256.clone(),
            benchmark_flags: cfg.benchmark_flags.clone(),
            benchmark_flags_sha256: cfg.benchmark_flags_sha256.clone(),
            requested_model_flags: cfg.requested_model_flags.clone(),
            requested_model_flags_sha256: cfg.requested_model_flags_sha256.clone(),
            requested_runtime_flags: cfg.requested_runtime_flags.clone(),
            requested_runtime_flags_sha256: cfg.requested_runtime_flags_sha256.clone(),
            requested_benchmark_flags: cfg.requested_benchmark_flags.clone(),
            requested_benchmark_flags_sha256: cfg.requested_benchmark_flags_sha256.clone(),
            client_configuration: cfg.client_configuration.clone(),
            client_extras: cfg.client_extras.clone(),
            failed: Some(false),
            failed_reason: None,
            failure_source: None,
            failure_retriable: None,
            parameter_draft_length: get_param_draft_length(benchmark),
            parameter_dataset_subset: get_param_dataset_subset(benchmark),
            parameter_batch_size: benchmark.parameter_batch_size().map(BatchSize::as_i32),
            parameter_temperature: get_param_temperature(benchmark),
            parameter_top_p: get_param_top_p(benchmark),
            parameter_top_k: get_param_top_k(benchmark),
            parameter_min_p: get_param_min_p(benchmark),
            parameter_presence_penalty: get_param_presence_penalty(benchmark),
            parameter_repeat_penalty: get_param_repeat_penalty(benchmark),
            parameter_seed: get_param_seed(benchmark),
            parameter_prompt_profile: get_param_prompt_profile(benchmark),
            parameter_num_beams: get_param_num_beams(benchmark),
            parameter_beam_repetition_penalty: get_param_beam_repetition_penalty(benchmark),
            parameter_beam_length_penalty: get_param_beam_length_penalty(benchmark),
            benchmark_params: Some(benchmark_params.clone()),
            benchmark_params_sha256: Some(benchmark_params_sha256.clone()),
            device_time_ms: submission.wire.device_time_ms.map(i64::from),
            observation_vl_spec_decode_turns: obs_turns,
            observation_dataset_digest: dataset_digest.as_ref().map(|d| d.as_ref().to_string()),
            observation_dataset_digest_matched: dataset_digest_matched,
            runtime_speculative_enabled: speculative_enabled,
            observation_output_digest: output_digest.clone(),
            rescored_from: submission.rescored_from.as_ref().map(ToString::to_string),
            // Read off the plans, not the record: see `ScoredJob::rerunning`.
            rerun_of: None,
            run_submitted_at: run_submitted_at_us,
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

/// A failure body whose day partition is closed to writes: carried out of
/// `incoming/` with no rows, never recorded.
fn archive_locked_failure(failure: FailureSubmission) -> anyhow::Result<ScoredJob> {
    let day_key = warehouse::day_key_from_timestamp(failure.submitted_at.timestamp_micros())?;
    tracing::info!(
        job_id = %failure.job_id,
        benchmark_id = %failure.wire.benchmark_id,
        client_id = %failure.client_id,
        day_key = %day_key,
        submitted_at = %failure.submitted_at,
        "archiving failure submission unrecorded: day partition is past the warehouse lock window"
    );
    Ok(ScoredJob {
        job_id: failure.job_id.clone(),
        benchmark_id: failure.wire.benchmark_id.clone(),
        client_id: failure.client_id.clone(),
        day_key,
        metric_rows: Vec::new(),
        eval_sample_results: None,
    })
}

/// One row per metric the benchmark intended to produce, `value` NULL, so
/// failure rate and speed share a group-by. `result_id` is `{job_id}_{i}`, the
/// shape [`crate::requeue_eval`] parses.
fn record_failure(
    catalog: &HashMap<BenchmarkId, pipette_core::benchmark::Benchmark>,
    models: &PassTable<'_>,
    failure: FailureSubmission,
    names: &VaultNames,
) -> anyhow::Result<ScoredJob> {
    let benchmark_id = &failure.wire.benchmark_id;
    let client_id = &failure.client_id;

    let benchmark = catalog
        .get(benchmark_id)
        .ok_or_else(|| anyhow::anyhow!("benchmark {benchmark_id} not in catalog"))?;
    let benchmark_type = benchmark.benchmark_type();
    let (benchmark_params, benchmark_params_sha256) = benchmark_params_columns(benchmark)?;

    let source = failure_source(&failure);
    let day_key = warehouse::day_key_from_timestamp(failure.submitted_at.timestamp_micros())?;
    let submitted_at_us = failure.submitted_at.timestamp_micros();
    let scored_at_us = Utc::now().timestamp_micros();

    // `benchmark_flags` and `client_configuration` are the two a failure cannot
    // fill: `FailureInput` has no such fields.
    let (model_descriptor, model_vault_descriptor) = recorded_model(
        failure.wire.model_descriptor.as_deref(),
        names.alias.as_deref(),
    );
    let raw = RawConfig {
        model_descriptor,
        model_vault_descriptor,
        runtime_descriptor: failure.wire.runtime_descriptor.as_deref(),
        model_flags: failure.wire.model_flags.as_ref().map(AsRef::as_ref),
        runtime_flags: failure.wire.runtime_flags.as_ref().map(AsRef::as_ref),
        benchmark_flags: None,
        requested_model_flags: failure.requested_model_flags.clone(),
        requested_runtime_flags: failure.requested_runtime_flags.clone(),
        requested_benchmark_flags: failure.requested_benchmark_flags.clone(),
        client_configuration: None,
        client_extras: failure.wire.client_extras.as_deref(),
    };
    let cfg = CanonicalConfig::new(raw, &names.labels);

    let speculative_enabled = resolved_speculation(
        &benchmark.def,
        failure.wire.runtime_flags.as_ref().map(AsRef::as_ref),
    );
    // A failure body carries no `model_params_*` to fall back to.
    let intended = intended_metrics_of(&benchmark.def, speculative_enabled);
    let (model_params_total_millions, model_params_active_millions) = models
        .resolve(intended.len(), model_descriptor, model_vault_descriptor)
        .map_or((None, None), |(_, e)| (Some(e.total), Some(e.active)));

    let failed_reason = failure.wire.failure_reason.as_str().to_string();
    let rows: Vec<MetricRow> = intended
        .iter()
        .enumerate()
        .map(|(i, (metric, unit))| MetricRow {
            result_id: format!("{}_{i}", failure.job_id),
            job_id: Some(failure.job_id.to_string()),
            benchmark_id: benchmark_id.clone(),
            benchmark_type,
            metric: (*metric).to_string(),
            client_id: client_id.clone(),
            device_name: None,
            device_form_factor: None,
            device_os_name: None,
            device_os_version: None,
            device_os_build: None,
            device_os_security_patch: None,
            device_cpu_arch: None,
            device_os_family: None,
            device_chip_model: None,
            device_gpu_model: None,
            device_gpu_vram_bytes: None,
            device_npu_model: None,
            device_npu_vram_bytes: None,
            device_ram_bytes: None,
            device_battery_level: None,
            device_power_state: None,
            device_power_save_mode: None,
            device_android_cpuset: None,
            device_android_cpu_affinity_list: None,
            device_android_cpu_affinity_excludes_top_tier: None,
            device_apple_thermal_state_before: None,
            device_apple_thermal_state_after: None,
            device_apple_soc_temp_c_before: None,
            device_apple_soc_temp_c_after: None,
            device_android_thermal_status_before: None,
            device_android_thermal_status_after: None,
            device_android_thermal_headroom_before: None,
            device_android_thermal_headroom_after: None,
            device_android_thermal_sensors_before: None,
            device_android_thermal_sensors_after: None,
            device_linux_thermal_zones_before: None,
            device_linux_thermal_zones_after: None,
            device_hardware_readings: None,
            device_os_throttling_readings: None,
            readiness_gate_waits: None,
            model_name: failure.wire.model_name.clone().map(Into::into),
            model_quant: failure.wire.model_quant.clone().map(Into::into),
            model_params_total_millions,
            model_params_active_millions,
            model_flags: cfg.model_flags.clone(),
            model_flags_sha256: cfg.model_flags_sha256.clone(),
            runtime_name: failure.wire.runtime_name.clone().map(Into::into),
            runtime_version: failure.wire.runtime_version.clone().map(Into::into),
            runtime_flags: cfg.runtime_flags.clone(),
            runtime_flags_sha256: cfg.runtime_flags_sha256.clone(),
            runtime_cpu_variant: None,
            client_version: failure.wire.client_version.clone().map(Into::into),
            value: None,
            value_stddev: None,
            unit: (*unit).to_string(),
            submitted_at: submitted_at_us,
            scored_at: scored_at_us,
            parameter_prefill_tokens: get_param_prefill_tokens(benchmark),
            parameter_decode_tokens: get_param_decode_tokens(benchmark),
            parameter_eval_id: get_param_eval_id(benchmark),
            parameter_image_width: get_param_image_width(benchmark),
            parameter_image_height: get_param_image_height(benchmark),
            parameter_text_tokens: get_param_text_tokens(benchmark),
            parameter_num_images: get_param_num_images(benchmark),
            parameter_audio_duration_ms: get_param_audio_duration_ms(benchmark),
            parameter_audio_sample_rate_hz: get_param_audio_sample_rate_hz(benchmark),
            parameter_audio_channels: get_param_audio_channels(benchmark),
            observation_vl_throughput_prefill_tokens: None,
            observation_vl_throughput_image_tokens: None,
            observation_asr_throughput_prefill_tokens: None,
            observation_asr_throughput_audio_tokens: None,
            observation_asr_context_sizing: None,
            observation_asr_audio_tokens_source: None,
            observation_asr_beyond_model_audio_limit: None,
            observation_max_swap_bytes: None,
            observation_max_host_bytes: None,
            score_runtime_version: None,
            eval_metadata: None,
            model_descriptor: cfg.model_descriptor.clone(),
            runtime_descriptor: cfg.runtime_descriptor.clone(),
            model_descriptor_sha256: cfg.model_descriptor_sha256.clone(),
            model_vault_descriptor: cfg.model_vault_descriptor.clone(),
            runtime_descriptor_sha256: cfg.runtime_descriptor_sha256.clone(),
            benchmark_flags: cfg.benchmark_flags.clone(),
            benchmark_flags_sha256: cfg.benchmark_flags_sha256.clone(),
            requested_model_flags: cfg.requested_model_flags.clone(),
            requested_model_flags_sha256: cfg.requested_model_flags_sha256.clone(),
            requested_runtime_flags: cfg.requested_runtime_flags.clone(),
            requested_runtime_flags_sha256: cfg.requested_runtime_flags_sha256.clone(),
            requested_benchmark_flags: cfg.requested_benchmark_flags.clone(),
            requested_benchmark_flags_sha256: cfg.requested_benchmark_flags_sha256.clone(),
            client_configuration: cfg.client_configuration.clone(),
            client_extras: cfg.client_extras.clone(),
            failed: Some(true),
            failed_reason: Some(failed_reason.clone()),
            failure_source: source,
            failure_retriable: Some(failure.wire.retriable),
            parameter_draft_length: get_param_draft_length(benchmark),
            parameter_dataset_subset: get_param_dataset_subset(benchmark),
            parameter_batch_size: benchmark.parameter_batch_size().map(BatchSize::as_i32),
            parameter_temperature: get_param_temperature(benchmark),
            parameter_top_p: get_param_top_p(benchmark),
            parameter_top_k: get_param_top_k(benchmark),
            parameter_min_p: get_param_min_p(benchmark),
            parameter_presence_penalty: get_param_presence_penalty(benchmark),
            parameter_repeat_penalty: get_param_repeat_penalty(benchmark),
            parameter_seed: get_param_seed(benchmark),
            parameter_prompt_profile: get_param_prompt_profile(benchmark),
            parameter_num_beams: get_param_num_beams(benchmark),
            parameter_beam_repetition_penalty: get_param_beam_repetition_penalty(benchmark),
            parameter_beam_length_penalty: get_param_beam_length_penalty(benchmark),
            benchmark_params: Some(benchmark_params.clone()),
            benchmark_params_sha256: Some(benchmark_params_sha256.clone()),
            observation_vl_spec_decode_turns: None,
            observation_dataset_digest: None,
            observation_dataset_digest_matched: None,
            // Configuration, not a measurement: it says which cell failed.
            runtime_speculative_enabled: speculative_enabled,
            observation_output_digest: None,
            device_time_ms: failure.wire.device_time_ms.map(i64::from),
            rescored_from: None,
            rerun_of: None,
            // A failure is never a copy, so it records its own run.
            run_submitted_at: Some(submitted_at_us),
        })
        .collect();

    tracing::info!(
        job_id = %failure.job_id,
        benchmark_id = %failure.wire.benchmark_id,
        client_id = %failure.client_id,
        failure_source = ?source,
        retriable = failure.wire.retriable,
        rows = rows.len(),
        "recorded failure submission"
    );

    Ok(ScoredJob {
        job_id: failure.job_id.clone(),
        benchmark_id: benchmark_id.clone(),
        client_id: client_id.clone(),
        day_key,
        metric_rows: rows,
        eval_sample_results: None,
    })
}

/// The stamped field wins; an older body falls back on `client_id`. A legacy
/// synthetic body records NULL rather than a guess from the reason text.
fn failure_source(failure: &FailureSubmission) -> Option<FailureSource> {
    failure.failure_source.or_else(|| {
        (!warehouse::is_system_client(&failure.client_id)).then_some(FailureSource::Client)
    })
}

/// The model parameter table for one scoring pass, plus the models it could not
/// resolve, so the pass closes by naming what to add.
///
/// The `Mutex` is uncontended and never held across an `await`; it saves
/// threading `&mut` through six call layers.
struct PassTable<'a> {
    table: &'a ModelParamTable,
    unresolved: Mutex<UnresolvedRows>,
}

impl<'a> PassTable<'a> {
    fn new(table: &'a ModelParamTable) -> Self {
        Self {
            table,
            unresolved: Mutex::new(UnresolvedRows::default()),
        }
    }

    /// [`resolve_model_entry`] for the `rows` rows one submission is about to
    /// write, recording the outcome against the pass.
    fn resolve(
        &self,
        rows: usize,
        model_descriptor: Option<&str>,
        model_vault_descriptor: Option<&str>,
    ) -> Option<(String, ModelEntry)> {
        let resolved = resolve_model_entry(self.table, model_descriptor, model_vault_descriptor);
        let mut unresolved = self
            .unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match resolved {
            Some(_) => unresolved.record_resolved(rows),
            None => unresolved.record_miss(
                rows,
                model_descriptor
                    .and_then(crate::model_params::descriptor_key)
                    .as_deref(),
                model_descriptor,
            ),
        }
        resolved
    }

    /// Both stdout and `tracing`, so a cron shows it whatever `RUST_LOG` filters.
    fn report_unresolved(&self) {
        let unresolved = self
            .unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let names: Vec<&str> = unresolved.named.keys().map(String::as_str).collect();
        let rows: usize = unresolved.named.values().map(|m| m.rows).sum();
        if !names.is_empty() {
            println!(
                "{count} model(s) have no model-params row, so {rows} row(s) kept the parameter \
                 counts the client reported: {list}. Add a row for each, then backfill the \
                 rows with `fix-model-param`.",
                count = names.len(),
                list = names.join(", "),
            );
            tracing::warn!(
                models = ?names,
                rows,
                scanned_rows = unresolved.scanned,
                "models with no model-params row; their rows keep the client-reported \
                 parameter counts"
            );
        }
        // Separate event: a table entry does not fix these.
        if unresolved.unparsed.rows > 0 {
            println!(
                "{n} row(s) carry a model_descriptor this server cannot parse, so they kept the \
                 parameter counts the client reported. An entry in the model parameter table will \
                 not fix these: this build does not know the shape.",
                n = unresolved.unparsed.rows,
            );
            tracing::warn!(
                rows = unresolved.unparsed.rows,
                scanned_rows = unresolved.scanned,
                "rows with an unparseable model_descriptor; they keep the client-reported \
                 parameter counts"
            );
        }
        // Only at debug: on pre-descriptor history this is most rows, and not news.
        if unresolved.no_descriptor.rows > 0 {
            tracing::debug!(
                rows = unresolved.no_descriptor.rows,
                scanned_rows = unresolved.scanned,
                "rows with no model_descriptor; they keep the client-reported parameter counts"
            );
        }
    }
}

/// The most vault resources one submission's descriptors are labelled from. A
/// speculative vision model names three files and a runtime one.
const MAX_LABELLED_RESOURCES: usize = 8;

/// What the vault records behind one submission's descriptors give its rows.
#[derive(Default)]
struct VaultNames {
    alias: Option<String>,
    labels: Fill,
}

impl VaultNames {
    /// A record that is gone labels nothing; a failed read is an error, so the
    /// job waits for a run that can read it. `rescored_from` names the job a
    /// copy's descriptors were reported for.
    async fn read(
        stores: &Stores,
        job_id: &JobId,
        rescored_from: Option<&JobId>,
        model: Option<&str>,
        runtime: Option<&str>,
    ) -> anyhow::Result<Self> {
        let ran = rescored_from.unwrap_or(job_id);
        if !names_only_its_jobs_records(stores, ran, [model, runtime]).await? {
            tracing::warn!(
                job_id = %job_id,
                ran = %ran,
                "the job source is missing, unreadable or names fewer vault resources than the descriptors; labelling none"
            );
            return Ok(Self::default());
        }
        let vault = &*stores.vault;
        let mut ids: BTreeSet<VaultResourceId> = [model, runtime]
            .into_iter()
            .flatten()
            .flat_map(unlabelled_resources)
            .collect();
        // A descriptor is a device's text, so cap the reads it can cause.
        if ids.len() > MAX_LABELLED_RESOURCES {
            tracing::warn!(
                job_id = %job_id,
                resources = ids.len(),
                "the descriptors name too many vault resources to label"
            );
            ids.clear();
        }
        let records = futures::future::try_join_all(ids.into_iter().map(|id| async move {
            let record = vault.get_resource(&id).await?;
            anyhow::Ok((id, record))
        }))
        .await
        .context("reading the vault records that the descriptors name")?;
        // The alias reads the model's other records, so none is read twice.
        let known: KnownOrigins = records
            .iter()
            .map(|(id, record)| (id.clone(), record.as_ref().and_then(|r| r.origin.clone())))
            .collect();
        let alias = recorded_alias(vault, job_id, model, &known).await?;
        let records = records.into_iter().filter_map(|(_, record)| record);
        Ok(Self {
            alias,
            labels: Fill::every(records),
        })
    }
}

/// Whether the source of job `ran` ([`ran_source`]) names every vault resource
/// the descriptors name ([`job_names`]).
async fn names_only_its_jobs_records(
    stores: &Stores,
    ran: &JobId,
    descriptors: [Option<&str>; 2],
) -> anyhow::Result<bool> {
    let Some(reported) = reported_resources(descriptors.into_iter().flatten()) else {
        return Ok(false);
    };
    if reported.is_empty() {
        return Ok(true);
    }
    let source = ran_source(stores, ran).await?;
    Ok(job_names(source.as_ref(), &reported))
}

/// The public alias of the model that `job_id` reports, if it ran from the
/// vault as a copy of a public model ([`crate::vault_alias`]).
async fn recorded_alias(
    vault: &dyn VaultStore,
    job_id: &JobId,
    reported: Option<&str>,
    known: &KnownOrigins,
) -> anyhow::Result<Option<String>> {
    let alias = crate::vault_alias::model_alias(vault, reported, known)
        .await
        .context("reading the vault records that the model descriptor names")?;
    if alias.is_some() {
        tracing::info!(
            job_id = %job_id,
            "recording a vault model under the descriptor of its public origin"
        );
    }
    Ok(alias)
}

/// The `model_descriptor` and the `model_vault_descriptor` that a row records.
fn recorded_model<'a>(
    reported: Option<&'a str>,
    alias: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    match alias {
        Some(alias) => (Some(alias), reported),
        None => (reported, None),
    }
}

/// Resolve by the descriptors, never `model_name`: a client may set it to
/// anything. Both row kinds resolve here, so they agree.
fn resolve_model_entry(
    table: &ModelParamTable,
    model_descriptor: Option<&str>,
    model_vault_descriptor: Option<&str>,
) -> Option<(String, ModelEntry)> {
    table.resolve_recorded(model_descriptor, model_vault_descriptor)
}

/// Resolve `(total, active)` `model_params_*_millions` for the warehouse row.
/// The table wins over the submission, which corrects stale client values.
fn resolve_mill_params(
    models: &PassTable<'_>,
    rows: usize,
    submission: &SuccessSubmission,
    model_descriptor: Option<&str>,
    model_vault_descriptor: Option<&str>,
) -> (Option<i32>, Option<i32>) {
    let resolved = models.resolve(rows, model_descriptor, model_vault_descriptor);

    if let Some((resolved_via, entry)) = resolved {
        let submission_total = submission.wire.model_params_total_millions.map(i32::from);
        let submission_active = submission
            .wire
            .model_params_active_millions
            .map(i32::from)
            .or(submission_total);
        if submission_total != Some(entry.total) || submission_active != Some(entry.active) {
            tracing::info!(
                job_id = %submission.job_id,
                resolved_via = %resolved_via,
                submission_params_total = ?submission_total,
                submission_params_active = ?submission_active,
                params_table_total = entry.total,
                params_table_active = entry.active,
                "scorer normalizing mill_params to values from the model parameter table"
            );
        }
        return (Some(entry.total), Some(entry.active));
    }
    // Unknown model: trust the submission; a dense model's active is its total.
    let total = submission.wire.model_params_total_millions.map(i32::from);
    let active = submission
        .wire
        .model_params_active_millions
        .map(i32::from)
        .or(total);
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
    /// JSON `{"samples_failed": N}` when any eval sample crashed client-side.
    eval_metadata: Option<String>,
}

fn require<T>(value: Option<T>, field: &str) -> anyhow::Result<T> {
    value.ok_or_else(|| anyhow::anyhow!("missing {field}"))
}

/// Drops duplicate ids (first wins, for legacy bodies) and bails on none,
/// rather than report `accuracy = 0/0`.
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

/// Read off the server-stamped `requested_model_flags`, never the client's
/// opaque `model_flags`: an eval's and a vlm_eval's alike, where the plan
/// declares whether a reasoning model thinks.
fn requested_enable_thinking(submission: &SuccessSubmission) -> Option<bool> {
    let requested = submission.requested_model_flags.as_deref()?;
    serde_json::from_str::<ModelFlags>(requested)
        .inspect_err(|e| {
            tracing::warn!(
                job_id = %submission.job_id,
                error = %e,
                "requested model flags do not read as model flags; scoring without enable_thinking"
            );
        })
        .ok()?
        .enable_thinking()
}

/// A vlm_eval's thinking mode: the server-stamped one, else, when there is no
/// stamp at all, its recipe's. A run the server did not queue (`benchmarks run
/// --sync`) carries no stamp, and a client runs a catalog entry only in its
/// recipe's mode. A stamp that states no mode, or does not read, is the
/// stamp's answer, never the recipe's.
fn vlm_eval_thinking(
    submission: &SuccessSubmission,
    recipe: Option<&VlmEvalRecipe>,
) -> Option<bool> {
    match submission.requested_model_flags {
        Some(_) => requested_enable_thinking(submission),
        None => recipe.and_then(|r| r.enable_thinking),
    }
}

/// `vlm_eval` scored as `eval` is, plus the dataset's official headline as
/// `headline_score` (the scorer's `context["accuracy"]`: POPE's F1, MME's
/// score / 2800, OCRBench's / 1000, MMBench's circular accuracy). The rows
/// keep their messages' text but not their base64 images.
async fn score_vlm_eval(
    vlm: &VlmEvalBenchmark,
    submission: &SuccessSubmission,
    config: &Config,
    http_client: &reqwest::Client,
    eval_response: Option<scoring_service::ScoreResponse>,
) -> anyhow::Result<DerivedResult> {
    let lmms_task = &vlm.parameter_lmms_task;
    let prompt_profile = vlm.parameter_prompt_profile.as_ref();
    let dataset = vlm.dataset.as_deref();
    let completions = eval_completions(submission)?;
    let samples_failed = completions.iter().filter(|c| is_failed_sample(c)).count();
    // Checked here too, not only before `/score`: a score staged for one pin
    // is not written under an entry re-pinned since.
    let samples_digest = dataset
        .map(|dataset| ran_the_pinned_corpus(submission, dataset))
        .transpose()?;
    let body = match eval_response {
        Some(body) => body,
        None => {
            call_score_service(
                http_client,
                config,
                lmms_task.as_str(),
                &vlm.parameter_dataset_version,
                &completions,
                vlm_eval_thinking(submission, vlm.recipe.as_ref()),
                prompt_profile.map(VlmPromptProfile::as_str),
                samples_digest,
            )
            .await?
        }
    };
    let context_json =
        serde_json::to_string(&body.context).context("serializing the scorer's context map")?;
    // The headline is the result: a context without a ratio for it is a
    // scorer fault, and finalizing without one would publish only the
    // per-row accuracy most of these datasets do not report.
    let headline = body
        .context
        .get("accuracy")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| (0.0..=1.0).contains(value))
        .with_context(|| {
            format!("the scorer sent no headline ratio for {lmms_task}: context {context_json}")
        })?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a headline score is a ratio in [0, 1]; the warehouse column is f32"
    )]
    let headline = headline as f32;
    // The scorer judges coverage against its own copy of the dataset: one
    // that finds samples unanswered is not scoring the corpus this run did.
    let missing = body
        .context
        .get("samples_missing")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    anyhow::ensure!(
        missing == 0,
        "the scorer finds {missing} of {lmms_task}'s samples unanswered by job {}",
        submission.job_id
    );
    let mut scored_samples = body.scored_samples;
    for message in scored_samples.iter_mut().flat_map(|s| &mut s.messages) {
        message.content.drop_images();
    }
    let eval_sample_results = build_eval_sample_results(&completions, scored_samples)?;

    let total = eval_sample_results.len();
    let correct = eval_sample_results.iter().filter(|r| r.is_correct).count();
    let accuracy = correct as f32 / total as f32;
    tracing::info!(
        %lmms_task,
        correct,
        total,
        samples_failed,
        accuracy,
        headline,
        runtime_version = ?body.runtime_version,
        context = %context_json,
        "vlm_eval scored"
    );

    let metrics = vec![
        DerivedMetric {
            metric: "accuracy".to_string(),
            value: accuracy,
            value_stddev: None,
            unit: "ratio".to_string(),
        },
        DerivedMetric {
            metric: "headline_score".to_string(),
            value: headline,
            value_stddev: None,
            unit: "ratio".to_string(),
        },
    ];
    Ok(DerivedResult {
        metrics,
        eval_sample_results: Some(eval_sample_results),
        score_runtime_version: Some(body.runtime_version),
        eval_metadata: build_eval_metadata(samples_failed),
    })
}

/// `asr_eval` scored by the scorer's corpus error rate: one headline metric,
/// `wer` (lower is better), a ratio, or `error_rate` for a task this build
/// doesn't know. The scorer must count words, the only unit it scores in. Each
/// row keeps the scorer's `measures` and its messages' text, its clips' parts
/// emptied.
///
/// As for a vlm_eval, a result is an answer to every sample, or no result: a
/// submission without exactly one row per sample, none failed, is refused (by
/// the caller, on the rows as stored), and so is a scorer reply with no rate or one that counts samples unanswered.
/// Such a job stays queued and logged, and is never published.
async fn score_asr_eval(
    asr: &AsrEvalBenchmark,
    submission: &SuccessSubmission,
    config: &Config,
    http_client: &reqwest::Client,
    eval_response: Option<scoring_service::ScoreResponse>,
) -> anyhow::Result<DerivedResult> {
    let asr_task = &asr.parameter_asr_task;
    let completions = eval_completions(submission)?;
    // Checked here too, as for a vlm_eval: a score staged for one pin is not
    // written under an entry re-pinned since.
    let samples_digest = asr_ran_the_pinned_corpus(submission, asr)?;
    let body = match eval_response {
        Some(body) => body,
        None => {
            call_score_service(
                http_client,
                config,
                asr_task.as_str(),
                &asr.parameter_dataset_version,
                &completions,
                asr_eval_thinking(asr),
                Some(asr.parameter_prompt_profile.as_str()),
                Some(samples_digest),
            )
            .await?
        }
    };
    let context = &body.context;
    let context_json =
        serde_json::to_string(context).context("serializing the scorer's context map")?;
    // Words are the only unit the evals server scores in; it retired its
    // character error rate, so a scorer naming one is out of contract.
    let scored_unit = match context.get("error_unit").and_then(Value::as_str) {
        Some("word") => AsrErrorUnit::Word,
        other => anyhow::bail!("the scorer sent no known error_unit for {asr_task}: {other:?}"),
    };
    if let Some(expected) = asr_task.error_unit()
        && expected != scored_unit
    {
        anyhow::bail!("{asr_task} counts {expected} errors but the scorer counted {scored_unit}");
    }
    // The rate is the result: a reply without one (a dataset with no
    // reference word, which no shipped set is) is not finalized.
    let error_rate = context
        .get("error_rate")
        .and_then(Value::as_f64)
        .filter(|rate| rate.is_finite() && *rate >= 0.0)
        .with_context(|| {
            format!("the scorer sent no error rate for {asr_task}: context {context_json}")
        })?;
    // The scorer judges coverage against its own copy of the dataset, as for
    // a vlm_eval. Absent is none missing; present, it must be a count, or a
    // malformed reply would pass as complete.
    let missing = match context.get("samples_missing") {
        None => 0,
        Some(value) => value.as_u64().with_context(|| {
            format!("the scorer sent samples_missing = {value} for {asr_task}, not a count")
        })?,
    };
    anyhow::ensure!(
        missing == 0,
        "the scorer finds {missing} of {asr_task}'s samples unanswered by job {}",
        submission.job_id
    );
    let eval_metadata = build_asr_eval_metadata(asr_task, context);
    let runtime_version = body.runtime_version;
    let mut scored_samples = body.scored_samples;
    for message in scored_samples.iter_mut().flat_map(|s| &mut s.messages) {
        message.content.drop_media();
    }
    let eval_sample_results = build_eval_sample_results(&completions, scored_samples)?;
    tracing::info!(
        %asr_task,
        metric = asr_task.metric(),
        error_rate,
        total = eval_sample_results.len(),
        runtime_version = ?runtime_version,
        context = %context_json,
        "asr_eval scored"
    );

    #[expect(
        clippy::cast_possible_truncation,
        reason = "an error rate is a small ratio; the warehouse column is f32"
    )]
    let value = error_rate as f32;
    // A rate past f32's range would land in the column as infinity.
    anyhow::ensure!(
        value.is_finite(),
        "the scorer sent error_rate = {error_rate} for {asr_task}, past what the column holds"
    );
    Ok(DerivedResult {
        metrics: vec![DerivedMetric {
            metric: asr_task.metric().to_string(),
            value,
            value_stddev: None,
            unit: "ratio".to_string(),
        }],
        eval_sample_results: Some(eval_sample_results),
        score_runtime_version: Some(runtime_version),
        eval_metadata,
    })
}

/// An asr_eval's `eval_metadata`: the scorer's `error_unit` where the task is
/// one this build doesn't know, so its `error_rate` row still says what it
/// counts; and the samples the Open ASR Leaderboard's row filter excluded.
fn build_asr_eval_metadata(
    asr_task: &AsrEvalTask,
    context: &BTreeMap<String, Value>,
) -> Option<String> {
    let mut map = serde_json::Map::new();
    if let Some(value) = context.get("samples_excluded") {
        map.insert("samples_excluded".to_string(), value.clone());
    }
    if asr_task.error_unit().is_none()
        && let Some(unit) = context.get("error_unit")
    {
        map.insert("error_unit".to_string(), unit.clone());
    }
    (!map.is_empty()).then(|| Value::Object(map).to_string())
}

/// The only multi-minute step in the eval pipeline. Every completion, failed
/// ones included, is forwarded with its `stop_reason`.
#[expect(
    clippy::too_many_arguments,
    reason = "the request's fields, each from a different part of the submission"
)]
async fn call_score_service(
    http_client: &reqwest::Client,
    config: &Config,
    eval_id: &str,
    dataset_name: &str,
    completions: &[SampleCompletion],
    enable_thinking: Option<bool>,
    prompt_profile: Option<&str>,
    samples_digest: Option<String>,
) -> anyhow::Result<scoring_service::ScoreResponse> {
    let score_samples: Vec<ScoreRequestSample<'_>> = completions
        .iter()
        .map(|c| ScoreRequestSample {
            id: &c.id,
            completion: &c.completion,
            stop_reason: c.stop_reason.as_deref(),
        })
        .collect();
    tracing::info!(
        eval_id = %eval_id,
        dataset = %dataset_name,
        total = completions.len(),
        samples_failed = completions.iter().filter(|c| is_failed_sample(c)).count(),
        enable_thinking = ?enable_thinking,
        prompt_profile = ?prompt_profile,
        "calling evals server"
    );
    // `?` keeps `ScoringError` downcastable for `is_service_unreachable`.
    Ok(scoring_service::score(
        http_client,
        &config.evals_server_url,
        &ScoreRequest {
            eval_id,
            dataset_name,
            completions: &score_samples,
            enable_thinking,
            prompt_profile,
            samples_digest,
        },
    )
    .await?)
}

/// [`answers_every_sample`] at scoring, on the rows as stored: a body queued
/// or requeued without passing the submission route's check is held to it
/// too. A refusal leaves the job queued and logged, as any scoring error does.
fn answered_every_sample(
    def: &BenchmarkDefinition,
    submission: &SuccessSubmission,
) -> anyhow::Result<()> {
    answers_every_sample(def, submission.wire.completions.as_deref().unwrap_or(&[])).with_context(
        || {
            format!(
                "{} job {} is not scored",
                def.benchmark_type(),
                submission.job_id
            )
        },
    )
}

/// A vlm_eval's answers are to its corpus's prompts. They are scored only if
/// the run reports the corpus the entry pins, and then against the evals
/// server's samples for it: the returned digest goes to `/score`, which
/// refuses if those samples moved since. A refusal leaves the job queued and
/// logged on every pass, as any scoring error does.
fn ran_the_pinned_corpus(success: &SuccessSubmission, dataset: &Dataset) -> anyhow::Result<String> {
    ran_the_pinned(BenchmarkType::VlmEval, success, dataset)
}

/// [`ran_the_pinned_corpus`] for an asr_eval, whose every catalog entry pins
/// its corpus: one without a pin is not scored.
fn asr_ran_the_pinned_corpus(
    success: &SuccessSubmission,
    asr: &AsrEvalBenchmark,
) -> anyhow::Result<String> {
    let dataset = asr.dataset.as_deref().with_context(|| {
        format!(
            "asr_eval job {} is for an entry that pins no corpus",
            success.job_id
        )
    })?;
    ran_the_pinned(BenchmarkType::AsrEval, success, dataset)
}

fn ran_the_pinned(
    benchmark: BenchmarkType,
    success: &SuccessSubmission,
    dataset: &Dataset,
) -> anyhow::Result<String> {
    let job_id = &success.job_id;
    reports_the_pinned_corpus(success.wire.observation_dataset_digest.as_ref(), dataset)
        .with_context(|| format!("{benchmark} job {job_id} is not scored"))?;
    let DatasetUpstream::EvalsServer { identity, .. } = &dataset.upstream else {
        anyhow::bail!(
            "{benchmark} job {job_id}'s corpus pins {}, not the evals server's samples",
            dataset.upstream.reference()
        );
    };
    Ok(format!("sha256:{identity}"))
}

/// An asr_eval's thinking mode for `/score`: its recipe's, which the load
/// check holds to `false`. The scorer refuses `true` for ASR.
fn asr_eval_thinking(asr: &AsrEvalBenchmark) -> Option<bool> {
    asr.recipe
        .as_ref()
        .and_then(|recipe| recipe.enable_thinking)
}

/// `eval_response` is the already-fetched `/score` answer, so finalize does not
/// repeat the call; `None` calls the service inline.
async fn derive_metrics(
    benchmark: &Benchmark,
    submission: &SuccessSubmission,
    config: &Config,
    http_client: &reqwest::Client,
    eval_response: Option<scoring_service::ScoreResponse>,
) -> anyhow::Result<DerivedResult> {
    match &benchmark.def {
        BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            parameter_prefill_tokens,
            ..
        }) => {
            let prefill_time_ms: f32 =
                require(submission.wire.prefill_time_ms, "prefill_time_ms")?.into();
            let stddev = submission.wire.prefill_time_ms_stddev.map(f32::from);
            let throughput =
                parameter_prefill_tokens.into_inner() as f32 / prefill_time_ms * 1000.0;
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
        BenchmarkDefinition::DecodeThroughput(DecodeThroughputBenchmark {
            parameter_decode_tokens,
            ..
        }) => {
            let decode_time_ms: f32 =
                require(submission.wire.decode_time_ms, "decode_time_ms")?.into();
            let stddev = submission.wire.decode_time_ms_stddev.map(f32::from);
            let throughput = parameter_decode_tokens.into_inner() as f32 / decode_time_ms * 1000.0;
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
        BenchmarkDefinition::EndToEndLatency(_) => {
            let total_time_ms: f32 =
                require(submission.wire.total_time_ms, "total_time_ms")?.into();
            Ok(DerivedResult {
                metrics: vec![DerivedMetric {
                    metric: "end_to_end_latency".to_string(),
                    value: total_time_ms,
                    value_stddev: submission.wire.total_time_ms_stddev.map(f32::from),
                    unit: "ms".to_string(),
                }],
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrMaxMemory(_) => {
            let max_host =
                i64::from(require(submission.wire.max_host_bytes, "max_host_bytes")?) as f32;
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
                        value: i64::from(v) as f32,
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
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_eval_id,
            parameter_dataset_name,
            ..
        }) => {
            let completions = eval_completions(submission)?;
            let samples_failed = completions.iter().filter(|c| is_failed_sample(c)).count();

            let body = match eval_response {
                Some(body) => body,
                None => {
                    call_score_service(
                        http_client,
                        config,
                        parameter_eval_id.as_str(),
                        parameter_dataset_name,
                        &completions,
                        requested_enable_thinking(submission),
                        None,
                        None,
                    )
                    .await?
                }
            };

            let eval_sample_results = build_eval_sample_results(&completions, body.scored_samples)?;

            let total = eval_sample_results.len();
            let correct = eval_sample_results.iter().filter(|r| r.is_correct).count();
            // total > 0: empty completions bail above, and the scorer echoes every id.
            let accuracy = correct as f32 / total as f32;
            let context_json = serde_json::to_string(&body.context)
                .context("serializing the scorer's context map")?;
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

            // Over all samples, failed ones scored wrong; consumers can adjust
            // with `samples_failed` from `eval_metadata`.
            let metrics = vec![DerivedMetric {
                metric: "accuracy".to_string(),
                value: accuracy,
                value_stddev: None,
                unit: "ratio".to_string(),
            }];
            let eval_metadata = build_eval_metadata(samples_failed);

            Ok(DerivedResult {
                metrics,
                eval_sample_results: Some(eval_sample_results),
                score_runtime_version: Some(body.runtime_version),
                eval_metadata,
            })
        }
        // Held to one answer to each sample on the rows as stored, before
        // anything reads them.
        BenchmarkDefinition::VlmEval(vlm) => {
            answered_every_sample(&benchmark.def, submission)?;
            score_vlm_eval(vlm, submission, config, http_client, eval_response).await
        }
        BenchmarkDefinition::AsrEval(asr) => {
            answered_every_sample(&benchmark.def, submission)?;
            score_asr_eval(asr, submission, config, http_client, eval_response).await
        }
        BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_decode_tokens,
            ..
        }) => {
            let times = PromptDecodeTimes {
                prompt_tokens: i32::from(require(submission.wire.prompt_tokens, "prompt_tokens")?)
                    as f32,
                prompt_ms: require(submission.wire.prompt_ms, "prompt_ms")?.into(),
                prompt_ms_stddev: submission.wire.prompt_ms_stddev.map(f32::from),
                predicted_ms: require(submission.wire.predicted_ms, "predicted_ms")?.into(),
                predicted_ms_stddev: submission.wire.predicted_ms_stddev.map(f32::from),
                total_ms: submission.wire.total_time_ms.map(f32::from),
                total_ms_stddev: submission.wire.total_time_ms_stddev.map(f32::from),
            };
            // The prefill length (image + text + template) and its image-only
            // part are observations, not metrics; score_success records them off
            // the wire.
            Ok(DerivedResult {
                metrics: prompt_decode_metrics(&times, parameter_decode_tokens.into_inner()),
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            parameter_decode_tokens,
            ..
        }) => {
            let run = submission.wire.asr_throughput_measurements()?;
            let times = PromptDecodeTimes {
                prompt_tokens: i32::from(run.prompt_tokens) as f32,
                prompt_ms: run.prompt_ms.into(),
                prompt_ms_stddev: run.prompt_ms_stddev.map(f32::from),
                predicted_ms: run.predicted_ms.into(),
                predicted_ms_stddev: run.predicted_ms_stddev.map(f32::from),
                total_ms: run.total_time_ms.map(f32::from),
                total_ms_stddev: run.total_time_ms_stddev.map(f32::from),
            };
            Ok(DerivedResult {
                metrics: prompt_decode_metrics(&times, parameter_decode_tokens.into_inner()),
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
        BenchmarkDefinition::VlSpecDecode(_) => {
            let run = submission.wire.vl_spec_decode_counters()?;
            Ok(DerivedResult {
                metrics: vl_spec_decode_metrics(&run),
                eval_sample_results: None,
                score_runtime_version: None,
                eval_metadata: None,
            })
        }
    }
}

/// Six metrics in either mode, three more with the drafter on. Every
/// denominator is positive by the wire types and `SuccessInput::validate`.
fn vl_spec_decode_metrics(c: &VlSpecDecodeCounters) -> Vec<DerivedMetric> {
    let turns = c.turns as f32;
    let metric = |name: &str, value: f32, unit: &str| DerivedMetric {
        metric: name.to_string(),
        value,
        value_stddev: None,
        unit: unit.to_string(),
    };
    let mut metrics = vec![
        // On the runtime's `predicted_ms`: tok/s is the host's, not the transport's.
        metric(
            "decode_throughput",
            c.generated_tokens as f32 / c.predicted_ms * 1000.0,
            "tokens/sec",
        ),
        // Client clock, per request: e2e is measured, so it includes overhead.
        metric("ttft_ms", c.ttft_ms / turns, "ms"),
        metric("decode_ms", c.decode_ms / turns, "ms"),
        metric("e2e_ms", c.e2e_ms / turns, "ms"),
        // The same two on the runtime's own clock, the reference harness's
        // convention: comparable within one engine only.
        metric("runtime_ttft", c.prompt_ms / turns, "ms"),
        metric(
            "runtime_e2e_ms",
            (c.prompt_ms + c.predicted_ms) / turns,
            "ms",
        ),
    ];
    if let Some(d) = &c.drafter {
        metrics.extend([
            // Legacy figure, kept for stable rows. It reads high: `accepted`
            // includes drafts discarded at the stop token.
            metric(
                "accept_length",
                c.generated_tokens as f32 / (c.generated_tokens - d.accepted_tokens) as f32,
                "tokens",
            ),
            // The mean of each turn's `(generated − 1) / decode_forwards`, on
            // the runtime's own forward count: the column to read.
            metric(
                "accept_length_per_request",
                d.turn_accept_length_sum / turns,
                "tokens",
            ),
            metric(
                "draft_acceptance_rate",
                d.accepted_tokens as f32 / d.drafted_tokens as f32,
                "ratio",
            ),
        ]);
    }
    metrics
}

/// The corpus benchmark's speculation mode as the client resolved it. Gated on
/// the type so a stray flag stays out of a column whose contract says null.
fn resolved_speculation(def: &BenchmarkDefinition, runtime_flags: Option<&str>) -> Option<bool> {
    matches!(def, BenchmarkDefinition::VlSpecDecode(_))
        .then(|| crate::submission::speculative_enabled(runtime_flags))
        .flatten()
}

/// [`intended_metrics`] for a definition: an asr_eval's task names the unit.
/// A task this build doesn't know still intends its row, as `error_rate`, so
/// its failures are counted. Named as `AsrEvalTask::metric` names it.
fn intended_metrics_of(
    def: &BenchmarkDefinition,
    speculative: Option<bool>,
) -> &'static [(&'static str, &'static str)] {
    match def {
        BenchmarkDefinition::AsrEval(asr) => match asr.parameter_asr_task.error_unit() {
            Some(AsrErrorUnit::Word) => &[("wer", "ratio")],
            None => &[("error_rate", "ratio")],
        },
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_) => {
            intended_metrics(def.benchmark_type(), speculative)
        }
    }
}

/// What a prompt-then-decode run times. `vl_throughput` and `asr_throughput`
/// report the same quantities under different wire names.
struct PromptDecodeTimes {
    prompt_tokens: f32,
    prompt_ms: f32,
    prompt_ms_stddev: Option<f32>,
    predicted_ms: f32,
    predicted_ms_stddev: Option<f32>,
    /// The request's span on the client's clock, when the client sent it.
    total_ms: Option<f32>,
    total_ms_stddev: Option<f32>,
}

/// The metrics of a prompt-then-decode run, one definition for VL and ASR so a
/// formula fixed for one cannot miss the other and their rows read side by side:
/// the prefill and decode phases on the runtime's timers, and
/// `end_to_end_latency` on the client's clock when it was sent.
fn prompt_decode_metrics(times: &PromptDecodeTimes, decode_tokens: i32) -> Vec<DerivedMetric> {
    let PromptDecodeTimes {
        prompt_tokens,
        prompt_ms,
        prompt_ms_stddev,
        predicted_ms,
        predicted_ms_stddev,
        total_ms,
        total_ms_stddev,
    } = *times;
    let prefill_throughput = prompt_tokens / prompt_ms * 1000.0;
    let decode_throughput = decode_tokens as f32 / predicted_ms * 1000.0;
    let mut metrics = vec![
        DerivedMetric {
            metric: "ttft".to_string(),
            value: prompt_ms,
            value_stddev: prompt_ms_stddev,
            unit: "ms".to_string(),
        },
        DerivedMetric {
            metric: "prefill_throughput".to_string(),
            value: prefill_throughput,
            value_stddev: prompt_ms_stddev.map(|s| prefill_throughput * s / prompt_ms),
            unit: "tokens/sec".to_string(),
        },
        DerivedMetric {
            metric: "decode_throughput".to_string(),
            value: decode_throughput,
            value_stddev: predicted_ms_stddev.map(|s| decode_throughput * s / predicted_ms),
            unit: "tokens/sec".to_string(),
        },
    ];
    metrics.extend(total_ms.map(|total_ms| DerivedMetric {
        metric: "end_to_end_latency".to_string(),
        value: total_ms,
        value_stddev: total_ms_stddev,
        unit: "ms".to_string(),
    }));
    metrics
}

/// The `(metric, unit)` pairs a benchmark intends to produce, for failure rows.
/// Must agree with [`derive_metrics`], except `max_gpu_usage`/`max_npu_usage`,
/// which depend on the device a failure may not name, and VL's and ASR's
/// `end_to_end_latency`, which not every client sends.
pub fn intended_metrics(
    kind: BenchmarkType,
    speculative: Option<bool>,
) -> &'static [(&'static str, &'static str)] {
    const VL_SPEC_DECODE: [(&str, &str); 9] = [
        ("decode_throughput", "tokens/sec"),
        ("ttft_ms", "ms"),
        ("decode_ms", "ms"),
        ("e2e_ms", "ms"),
        ("runtime_ttft", "ms"),
        ("runtime_e2e_ms", "ms"),
        ("accept_length", "tokens"),
        ("accept_length_per_request", "tokens"),
        ("draft_acceptance_rate", "ratio"),
    ];
    match kind {
        BenchmarkType::PrefillThroughput => &[("ttft", "ms"), ("prefill_throughput", "tokens/sec")],
        BenchmarkType::DecodeThroughput => &[("decode_throughput", "tokens/sec")],
        BenchmarkType::EndToEndLatency => &[("end_to_end_latency", "ms")],
        BenchmarkType::MaxMemoryUsage
        | BenchmarkType::VlMaxMemory
        | BenchmarkType::AsrMaxMemory => &[("max_host_usage", "bytes")],
        BenchmarkType::Eval => &[("accuracy", "ratio")],
        BenchmarkType::VlmEval => &[("accuracy", "ratio"), ("headline_score", "ratio")],
        // Every task this build knows counts words. A row's own task names
        // its unit, and an unknown one intends `error_rate`
        // ([`intended_metrics_of`]).
        BenchmarkType::AsrEval => &[("wer", "ratio")],
        BenchmarkType::VlThroughput | BenchmarkType::AsrThroughput => &[
            ("ttft", "ms"),
            ("prefill_throughput", "tokens/sec"),
            ("decode_throughput", "tokens/sec"),
        ],
        // The drafter's three only when known to be on: a failure may not have
        // resolved the mode.
        BenchmarkType::VlSpecDecode if speculative == Some(true) => &VL_SPEC_DECODE,
        BenchmarkType::VlSpecDecode => &VL_SPEC_DECODE[..6],
    }
}

/// The upstream subset the benchmark's corpus keeps; `None` for the whole
/// upstream and for every type that serves no corpus.
fn get_param_dataset_subset(benchmark: &Benchmark) -> Option<String> {
    benchmark.dataset().and_then(|d| d.subset.clone())
}

/// `stop_reason == "failure"` is the sole signal, not the legacy `failed` flag.
fn is_failed_sample(c: &SampleCompletion) -> bool {
    c.stop_reason.as_deref() == Some("failure")
}

/// Re-inject the client's per-sample metadata onto each scored sample.
///
/// The scorer must echo every forwarded id exactly once; otherwise bail, so
/// the job stays in `incoming/` rather than mask a contract violation.
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
                    let stop_reason = c.stop_reason.clone();
                    let is_failure = stop_reason.as_deref() == Some("failure");
                    // Falls back so the crash detail survives older clients.
                    let stop_detail = if is_failure {
                        c.stop_detail.clone().or_else(|| c.failed_reason.clone())
                    } else {
                        c.stop_detail.clone()
                    };
                    // Derived from `stop_reason`, not the flag, so they can't disagree.
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
                    // An id we never sent; keep going so the bail names them all.
                    unknown.push(s.id.clone().into_inner());
                    (false, None, None, None, None)
                }
            };
            // `derived` is reserved for backfill jobs.
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
                measures: s.measures.as_ref().map(serde_json::Value::to_string),
                chat_template_kwargs: s
                    .chat_template_kwargs
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
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

/// `None` rather than an empty `{}` blob.
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

/// The decoding columns an asr_eval row records: the decoding the entry ran
/// ([`AsrEvalBenchmark::effective_decoding`]), each knob it leaves unpinned at
/// its profile card's value. A beam search samples nothing, so its sampling
/// columns are NULL, and a sampled entry's beam columns are. A profile this
/// build has no card for records only what the entry pins.
#[derive(Debug, Default, PartialEq)]
struct AsrDecodingColumns {
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<i32>,
    min_p: Option<f64>,
    repeat_penalty: Option<f64>,
    seed: Option<i64>,
    num_beams: Option<i32>,
    beam_repetition_penalty: Option<f64>,
    beam_length_penalty: Option<f64>,
}

fn asr_decoding_columns(asr: &AsrEvalBenchmark) -> AsrDecodingColumns {
    match asr.effective_decoding() {
        Ok(AsrDecoding::Beams(beams)) => AsrDecodingColumns {
            num_beams: Some(beams.num_beams.into()),
            beam_repetition_penalty: Some(beams.repetition_penalty.as_f64()),
            beam_length_penalty: Some(beams.length_penalty.as_f64()),
            ..AsrDecodingColumns::default()
        },
        Ok(AsrDecoding::Sampling(sampling)) => AsrDecodingColumns {
            temperature: Some(sampling.temperature.as_f64()),
            top_p: Some(sampling.top_p.as_f64()),
            top_k: Some(sampling.top_k.into()),
            min_p: Some(sampling.min_p.as_f64()),
            repeat_penalty: Some(sampling.repeat_penalty.as_f64()),
            seed: sampling.seed.map(Into::into),
            ..AsrDecodingColumns::default()
        },
        Err(_) => AsrDecodingColumns {
            temperature: asr.parameter_temperature.map(Into::into),
            top_p: asr.parameter_top_p.map(Into::into),
            top_k: asr.parameter_top_k.map(Into::into),
            min_p: asr.parameter_min_p.map(Into::into),
            repeat_penalty: asr.parameter_repeat_penalty.map(Into::into),
            seed: asr.parameter_seed.map(Into::into),
            num_beams: asr.parameter_num_beams.map(Into::into),
            beam_repetition_penalty: asr.parameter_beam_repetition_penalty.map(|p| p.as_f64()),
            beam_length_penalty: asr.parameter_beam_length_penalty.map(|p| p.as_f64()),
        },
    }
}

/// An asr_eval's beam width, `None` where it decodes without beams.
fn get_param_num_beams(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).num_beams,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_beam_repetition_penalty(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).beam_repetition_penalty,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_beam_length_penalty(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).beam_length_penalty,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_prefill_tokens(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            parameter_prefill_tokens,
            ..
        })
        | BenchmarkDefinition::DecodeThroughput(DecodeThroughputBenchmark {
            parameter_prefill_tokens,
            ..
        })
        | BenchmarkDefinition::EndToEndLatency(EndToEndLatencyBenchmark {
            parameter_prefill_tokens,
            ..
        })
        | BenchmarkDefinition::MaxMemoryUsage(MaxMemoryUsageBenchmark {
            parameter_prefill_tokens,
            ..
        }) => Some(parameter_prefill_tokens.into_inner()),
        BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_decode_tokens(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::DecodeThroughput(DecodeThroughputBenchmark {
            parameter_decode_tokens,
            ..
        })
        | BenchmarkDefinition::EndToEndLatency(EndToEndLatencyBenchmark {
            parameter_decode_tokens,
            ..
        })
        | BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_decode_tokens,
            ..
        }) => Some(parameter_decode_tokens.into_inner()),
        BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            parameter_decode_tokens,
            ..
        })
        | BenchmarkDefinition::AsrMaxMemory(AsrMaxMemoryBenchmark {
            parameter_decode_tokens,
            ..
        }) => Some(parameter_decode_tokens.into_inner()),
        BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            parameter_decode_tokens,
            ..
        }) => parameter_decode_tokens.map(Into::into),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_eval_id(benchmark: &Benchmark) -> Option<String> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_eval_id, ..
        }) => Some(parameter_eval_id.to_string()),
        BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_lmms_task,
            ..
        }) => Some(parameter_lmms_task.to_string()),
        BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
            parameter_asr_task, ..
        }) => Some(parameter_asr_task.to_string()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_image_width(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_image_width,
            ..
        })
        | BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            parameter_image_width,
            ..
        }) => Some(parameter_image_width.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_image_height(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_image_height,
            ..
        })
        | BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            parameter_image_height,
            ..
        }) => Some(parameter_image_height.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_text_tokens(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_text_tokens,
            ..
        })
        | BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            parameter_text_tokens,
            ..
        })
        | BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            parameter_text_tokens,
            ..
        })
        | BenchmarkDefinition::AsrMaxMemory(AsrMaxMemoryBenchmark {
            parameter_text_tokens,
            ..
        }) => Some(parameter_text_tokens.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_temperature(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_temperature,
            ..
        })
        | BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_temperature,
            ..
        }) => parameter_temperature.map(Into::into),
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).temperature,
        BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
            parameter_temperature,
            ..
        }) => Some(parameter_temperature.as_f64()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlMaxMemory(_) => None,
    }
}

fn get_param_top_p(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_top_p, ..
        })
        | BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_top_p, ..
        }) => parameter_top_p.map(Into::into),
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).top_p,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_top_k(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_top_k, ..
        })
        | BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_top_k, ..
        }) => parameter_top_k.map(Into::into),
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).top_k,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_min_p(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_min_p, ..
        })
        | BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_min_p, ..
        }) => parameter_min_p.map(Into::into),
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).min_p,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

/// The `benchmark_params` column and its content id. The type tag stays inside;
/// the corpus does not, so entries differing only in corpus share the id.
fn benchmark_params_columns(benchmark: &Benchmark) -> anyhow::Result<(String, String)> {
    let params = benchmark.params();
    // plan-types' rule, so a client reproduces the id from its own copy.
    let canonical = pipette_plan_types::canonical_json(&serde_json::to_value(&params)?);
    let sha256 = pipette_plan_types::descriptor_digest(&params)?;
    Ok((canonical, sha256))
}

fn get_param_seed(benchmark: &Benchmark) -> Option<i64> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(EvalBenchmark { parameter_seed, .. })
        | BenchmarkDefinition::VlmEval(VlmEvalBenchmark { parameter_seed, .. }) => {
            parameter_seed.map(Into::into)
        }
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).seed,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_presence_penalty(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_presence_penalty,
            ..
        }) => parameter_presence_penalty.map(Into::into),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_repeat_penalty(benchmark: &Benchmark) -> Option<f64> {
    match &benchmark.def {
        BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_repeat_penalty,
            ..
        }) => parameter_repeat_penalty.map(Into::into),
        BenchmarkDefinition::AsrEval(asr) => asr_decoding_columns(asr).repeat_penalty,
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_prompt_profile(benchmark: &Benchmark) -> Option<String> {
    match &benchmark.def {
        BenchmarkDefinition::VlmEval(VlmEvalBenchmark {
            parameter_prompt_profile,
            ..
        }) => parameter_prompt_profile.as_ref().map(ToString::to_string),
        BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
            parameter_prompt_profile,
            ..
        }) => Some(parameter_prompt_profile.to_string()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_draft_length(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
            parameter_draft_length,
            ..
        }) => Some(parameter_draft_length.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlMaxMemory(_) => None,
    }
}

fn get_param_audio_sample_rate_hz(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            parameter_audio_sample_rate_hz,
            ..
        })
        | BenchmarkDefinition::AsrMaxMemory(AsrMaxMemoryBenchmark {
            parameter_audio_sample_rate_hz,
            ..
        }) => Some(parameter_audio_sample_rate_hz.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_audio_channels(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            parameter_audio_channels,
            ..
        })
        | BenchmarkDefinition::AsrMaxMemory(AsrMaxMemoryBenchmark {
            parameter_audio_channels,
            ..
        }) => Some(i32::from(parameter_audio_channels.into_inner())),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_audio_duration_ms(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            parameter_audio_duration_ms,
            ..
        })
        | BenchmarkDefinition::AsrMaxMemory(AsrMaxMemoryBenchmark {
            parameter_audio_duration_ms,
            ..
        }) => Some(parameter_audio_duration_ms.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

fn get_param_num_images(benchmark: &Benchmark) -> Option<i32> {
    match &benchmark.def {
        BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_num_images,
            ..
        })
        | BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            parameter_num_images,
            ..
        }) => Some(parameter_num_images.into_inner()),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::{Value, json};

    use pipette_core::benchmark::Benchmark;
    use pipette_plan_types::{
        AsrDecodeTokens, AudioChannels, AudioDurationMillis, AudioSampleRate, NonNegativeCount,
        PositiveCount, SamplingTemperature,
    };

    use crate::score::*;
    use crate::submission::{FIXTURE_DIGEST, vl_spec_decode_benchmark, vl_spec_decode_wire};

    /// The tests call only loopback mocks, so the client skips the slow
    /// system proxy lookup (see [`crate::http_client::builder_for`]).
    fn test_client() -> reqwest::Result<reqwest::Client> {
        reqwest::Client::builder().no_proxy().build()
    }

    fn test_submission(extras: Value) -> anyhow::Result<SuccessSubmission> {
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
            base.as_object_mut()
                .context("the fixture body is a JSON object")?
                .extend(map);
        }
        serde_json::from_value(base).context("test_submission should deserialize")
    }

    fn metric_value(derived: &DerivedResult, name: &str) -> anyhow::Result<f32> {
        derived
            .metrics
            .iter()
            .find(|m| m.metric == name)
            .map(|m| m.value)
            .with_context(|| format!("derive_metrics did not produce {name}"))
    }

    async fn derive_vl_spec_decode(speculative: bool) -> anyhow::Result<DerivedResult> {
        derive_metrics(
            &vl_spec_decode_benchmark()?,
            &test_submission(vl_spec_decode_wire(speculative))?,
            &Config::default(),
            &test_client()?,
            None,
        )
        .await
    }

    fn assert_near(derived: &DerivedResult, name: &str, want: f32) -> anyhow::Result<()> {
        let got = metric_value(derived, name)?;
        anyhow::ensure!(
            (got - want).abs() < 0.01,
            "{name}: expected {want}, got {got}"
        );
        Ok(())
    }

    /// No drafter metric, so no `0 / 0` acceptance rate. The fixture spends
    /// 10 ms per request outside `ttft + decode`, pinning e2e to the client clock.
    #[tokio::test]
    async fn test_derive_vl_spec_decode_metrics_speculation_off() -> anyhow::Result<()> {
        let derived = derive_vl_spec_decode(false).await?;
        let names: Vec<&str> = derived.metrics.iter().map(|m| m.metric.as_str()).collect();
        assert_eq!(
            names,
            [
                "decode_throughput",
                "ttft_ms",
                "decode_ms",
                "e2e_ms",
                "runtime_ttft",
                "runtime_e2e_ms"
            ]
        );
        assert_eq!(
            metric_value(&derived, "decode_throughput")?,
            14_000.0 / 226_000.0 * 1000.0
        );
        assert_near(&derived, "ttft_ms", 110.0)?;
        assert_near(&derived, "decode_ms", 315.0)?;
        // 435, not the 425 that `ttft + decode` would give.
        assert_near(&derived, "e2e_ms", 435.0)?;
        assert_near(&derived, "runtime_ttft", 79_000.0 / 723.0)?;
        assert_near(&derived, "runtime_e2e_ms", 305_000.0 / 723.0)?;
        assert!(derived.eval_sample_results.is_none());
        assert!(derived.score_runtime_version.is_none());
        Ok(())
    }

    /// The fixture makes the two accept-length columns coincide; on a real row
    /// they differ by construction.
    #[tokio::test]
    async fn test_derive_vl_spec_decode_metrics_speculation_on() -> anyhow::Result<()> {
        let derived = derive_vl_spec_decode(true).await?;
        let names: Vec<&str> = derived.metrics.iter().map(|m| m.metric.as_str()).collect();
        assert_eq!(
            names,
            [
                "decode_throughput",
                "ttft_ms",
                "decode_ms",
                "e2e_ms",
                "runtime_ttft",
                "runtime_e2e_ms",
                "accept_length",
                "accept_length_per_request",
                "draft_acceptance_rate"
            ]
        );
        assert_eq!(metric_value(&derived, "decode_throughput")?, 140.0);
        assert_near(&derived, "ttft_ms", 110.0)?;
        assert_near(&derived, "decode_ms", 140.0)?;
        assert_near(&derived, "e2e_ms", 260.0)?;
        assert_near(&derived, "runtime_e2e_ms", 179_000.0 / 723.0)?;
        assert_eq!(metric_value(&derived, "accept_length")?, 4.0);
        assert_eq!(metric_value(&derived, "accept_length_per_request")?, 4.0);
        assert_eq!(metric_value(&derived, "draft_acceptance_rate")?, 0.375);
        Ok(())
    }

    /// The plain `prefill_throughput_256` benchmark at batch size 1.
    fn prefill_benchmark() -> anyhow::Result<Benchmark> {
        Ok(Benchmark {
            org_id: None,
            def: BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new("prefill_throughput_256")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
                parameter_batch_size: BatchSize::one(),
            }),
        })
    }

    async fn score_one(benchmark: Benchmark, wire: Value) -> anyhow::Result<Vec<MetricRow>> {
        let mut body = wire;
        body.as_object_mut()
            .context("the fixture body is a JSON object")?
            .insert(
                "benchmark_id".to_string(),
                json!(benchmark.benchmark_id().as_str()),
            );
        let catalog = HashMap::from([(benchmark.benchmark_id().clone(), benchmark)]);
        let model_table = crate::model_params::ModelParamTable::default();
        let models = PassTable::new(&model_table);
        let submission = test_submission(body)?;
        let scored = score_success(
            &Config::default(),
            &catalog,
            &models,
            &test_client()?,
            submission,
            None,
            &VaultNames::default(),
        )
        .await?;
        Ok(scored.metric_rows)
    }

    #[tokio::test]
    async fn test_device_arch_and_os_family_reach_metric_rows() -> anyhow::Result<()> {
        let benchmark = prefill_benchmark()?;
        let rows = score_one(
            benchmark,
            json!({
                "prefill_time_ms": 34.7,
                "device_cpu_arch": "amd64",
                "device_os_family": "Linux"
            }),
        )
        .await?;
        assert!(!rows.is_empty());
        for row in rows {
            assert_eq!(row.device_cpu_arch.as_deref(), Some("amd64"));
            assert_eq!(row.device_os_family.as_deref(), Some("Linux"));
        }
        Ok(())
    }

    /// `None` (no digest reported) is not `Some(false)` (the wrong corpus), and
    /// a mismatch is recorded, not rejected.
    #[rstest]
    #[case::matched(Some(FIXTURE_DIGEST.to_string()), Some(true))]
    #[case::mismatch(Some("22".repeat(32)), Some(false))]
    #[case::unreported(None, None)]
    #[tokio::test]
    async fn test_vl_spec_decode_records_the_dataset_digest_verdict(
        #[case] reported: Option<String>,
        #[case] expected: Option<bool>,
    ) -> anyhow::Result<()> {
        let mut wire = vl_spec_decode_wire(true);
        let obj = wire
            .as_object_mut()
            .context("the fixture body is a JSON object")?;
        if let Some(digest) = &reported {
            obj.insert("observation_dataset_digest".to_string(), json!(digest));
        }

        let rows = score_one(vl_spec_decode_benchmark()?, wire).await?;

        anyhow::ensure!(!rows.is_empty(), "scoring produced no rows");
        rows.iter().for_each(|row| {
            assert_eq!(row.observation_dataset_digest, reported);
            assert_eq!(row.observation_dataset_digest_matched, expected);
            assert_eq!(row.failed, Some(false), "a digest verdict is not a failure");
            assert!(row.value.is_some(), "the measurement is still recorded");
        });
        Ok(())
    }

    /// `runtime_speculative_enabled = false` must not read as NULL.
    #[rstest]
    #[case::off(false, 6)]
    #[case::on(true, 9)]
    #[tokio::test]
    async fn test_vl_spec_decode_stamps_the_pairing_columns(
        #[case] speculative: bool,
        #[case] metrics: usize,
    ) -> anyhow::Result<()> {
        let rows = score_one(
            vl_spec_decode_benchmark()?,
            vl_spec_decode_wire(speculative),
        )
        .await?;
        assert_eq!(rows.len(), metrics);
        let digest = "3".repeat(64);
        rows.iter().for_each(|row| {
            assert_eq!(row.runtime_speculative_enabled, Some(speculative));
            assert_eq!(row.observation_vl_spec_decode_turns, Some(723));
            assert_eq!(
                row.observation_output_digest.as_deref(),
                Some(digest.as_str())
            );
            // One job is one dataset, so a per-dataset chart is a `GROUP BY`.
            assert_eq!(row.parameter_dataset_subset.as_deref(), Some("GQA"));
            assert_eq!(row.parameter_draft_length, Some(8));
            assert_eq!(row.parameter_batch_size, Some(1));
            assert_eq!(row.parameter_temperature, Some(0.0));
        });
        let ids: HashSet<&str> = rows.iter().map(|r| r.result_id.as_str()).collect();
        assert_eq!(ids.len(), rows.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_vl_spec_decode_stamps_a_batched_benchmark() -> anyhow::Result<()> {
        let mut benchmark = vl_spec_decode_benchmark()?;
        if let BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
            parameter_batch_size,
            ..
        }) = &mut benchmark.def
        {
            *parameter_batch_size = BatchSize::try_new(8)?;
        }

        let rows = score_one(benchmark, vl_spec_decode_wire(true)).await?;

        assert_eq!(rows.len(), 9);
        assert!(rows.iter().all(|r| r.parameter_batch_size == Some(8)));
        Ok(())
    }

    /// Never NULL here, unlike `eval`: the entry always carries a temperature.
    #[rstest]
    #[case::greedy(0.0)]
    #[case::sampled(0.8)]
    #[tokio::test]
    async fn test_vl_spec_decode_stamps_its_temperature(
        #[case] temperature: f64,
    ) -> anyhow::Result<()> {
        let mut benchmark = vl_spec_decode_benchmark()?;
        if let BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
            parameter_temperature,
            ..
        }) = &mut benchmark.def
        {
            *parameter_temperature = SamplingTemperature::try_new(temperature)?;
        }

        let rows = score_one(benchmark, vl_spec_decode_wire(true)).await?;

        assert_eq!(rows.len(), 9);
        assert!(
            rows.iter()
                .all(|r| r.parameter_temperature == Some(temperature))
        );
        Ok(())
    }

    /// Built through the loader, so a case authors `pinned` as real TOML.
    fn eval_benchmark(id: &str, pinned: &str) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            id,
            &format!(
                "benchmark_type = \"eval\"\n\
                 parameter_eval_id = \"math_500\"\n\
                 parameter_dataset_name = \"2026.06.1\"\n\
                 parameter_max_tokens = 8192\n{pinned}\n"
            ),
        )
    }

    /// A failure keeps the table's parameters, so it still groups with the
    /// sampling it was asked to run.
    #[rstest]
    #[case::greedy(
        "eval_math_500_2026.06.1_t0",
        "parameter_temperature = 0.0",
        Some(0.0),
        None,
        None,
        None,
        None
    )]
    #[case::full(
        "eval_math_500_2026.06.1_t1_topp0.95_topk64_seed7",
        "parameter_temperature = 1.0\nparameter_top_p = 0.95\nparameter_top_k = 64\n\
         parameter_min_p = 0.0\nparameter_seed = 7",
        Some(1.0),
        Some(0.95),
        Some(64),
        Some(0.0),
        Some(7)
    )]
    #[case::unpinned("eval_math_500_2026.06.1", "", None, None, None, None, None)]
    fn test_eval_failure_rows_carry_the_sampling(
        #[case] id: &str,
        #[case] pinned: &str,
        #[case] temperature: Option<f64>,
        #[case] top_p: Option<f64>,
        #[case] top_k: Option<i32>,
        #[case] min_p: Option<f64>,
        #[case] seed: Option<i64>,
    ) -> anyhow::Result<()> {
        let rows = failure_rows(
            eval_benchmark(id, pinned)?,
            json!({
                "benchmark_type": "eval",
                "retriable": true,
                "failure_reason": "llama-server exited mid-eval",
            }),
            None,
        )?;

        assert!(!rows.is_empty());
        rows.iter().for_each(|r| {
            assert_eq!(r.failed, Some(true), "row {}", r.result_id);
            assert_eq!(
                (
                    r.parameter_temperature,
                    r.parameter_top_p,
                    r.parameter_top_k,
                    r.parameter_min_p,
                    r.parameter_seed
                ),
                (temperature, top_p, top_k, min_p, seed),
                "row {}",
                r.result_id
            );
        });
        Ok(())
    }

    /// An unpinned temperature takes the eval-id rule's; a whole float keeps its
    /// `.0`, and `-0.0` reads as `0.0`.
    #[rstest]
    #[case::greedy(
        "eval_math_500_2026.06.1_t0",
        "parameter_temperature = 0.0",
        r#"{"benchmark_type":"eval","dataset_name":"2026.06.1","eval_id":"math_500","max_tokens":8192,"temperature":0.0}"#
    )]
    #[case::full(
        "eval_math_500_2026.06.1_t1_topp0.95_topk64_seed7",
        "parameter_temperature = 1\nparameter_top_p = 0.95\nparameter_top_k = 64\n\
         parameter_min_p = 0.0\nparameter_seed = 7",
        r#"{"benchmark_type":"eval","dataset_name":"2026.06.1","eval_id":"math_500","max_tokens":8192,"min_p":0.0,"seed":7,"temperature":1.0,"top_k":64,"top_p":0.95}"#
    )]
    #[case::unpinned(
        "eval_math_500_2026.06.1",
        "",
        r#"{"benchmark_type":"eval","dataset_name":"2026.06.1","eval_id":"math_500","max_tokens":8192,"temperature":0.6}"#
    )]
    #[case::negative_zero(
        "eval_math_500_2026.06.1_t0",
        "parameter_temperature = -0.0\nparameter_min_p = -0.0",
        r#"{"benchmark_type":"eval","dataset_name":"2026.06.1","eval_id":"math_500","max_tokens":8192,"min_p":0.0,"temperature":0.0}"#
    )]
    // The `-0.0` rule leaves an integer zero an integer.
    #[case::integer_zero(
        "eval_math_500_2026.06.1_t0_topk0_seed0",
        "parameter_temperature = 0.0\nparameter_top_k = 0\nparameter_seed = 0",
        r#"{"benchmark_type":"eval","dataset_name":"2026.06.1","eval_id":"math_500","max_tokens":8192,"seed":0,"temperature":0.0,"top_k":0}"#
    )]
    fn test_eval_failure_rows_carry_the_pinned_params(
        #[case] id: &str,
        #[case] pinned: &str,
        #[case] params: &str,
    ) -> anyhow::Result<()> {
        let rows = failure_rows(
            eval_benchmark(id, pinned)?,
            json!({
                "benchmark_type": "eval",
                "retriable": true,
                "failure_reason": "llama-server exited mid-eval",
            }),
            None,
        )?;

        assert!(!rows.is_empty());
        let off: Vec<_> = rows
            .iter()
            .filter(|r| r.benchmark_params.as_deref() != Some(params))
            .map(|r| r.result_id.as_str())
            .collect();
        assert!(off.is_empty(), "rows without the descriptor: {off:?}");
        Ok(())
    }

    /// A vlm_eval stamps what its entry pins, and an unpinned one, greedy by
    /// rule, stamps nothing: its descriptor is the one it had before the knobs.
    #[rstest]
    #[case::unpinned(
        "",
        [None; 6],
        None,
        r#"{"benchmark_type":"vlm_eval","dataset_version":"2026.10.1","lmms_task":"mmstar","max_tokens":8192,"sample_count":1500}"#
    )]
    #[case::model_card(
        "parameter_temperature = 0.7\nparameter_top_p = 0.8\nparameter_top_k = 20\n\
         parameter_min_p = 0.0\nparameter_presence_penalty = 1.5\n\
         parameter_repeat_penalty = 1.0\nparameter_seed = 7",
        [Some(0.7), Some(0.8), Some(20.0), Some(0.0), Some(1.5), Some(1.0)],
        Some(7),
        r#"{"benchmark_type":"vlm_eval","dataset_version":"2026.10.1","lmms_task":"mmstar","max_tokens":8192,"min_p":0.0,"presence_penalty":1.5,"repeat_penalty":1.0,"sample_count":1500,"seed":7,"temperature":0.7,"top_k":20,"top_p":0.8}"#
    )]
    fn test_vlm_eval_failure_rows_carry_the_sampling(
        #[case] pinned: &str,
        #[case] knobs: [Option<f64>; 6],
        #[case] seed: Option<i64>,
        #[case] params: &str,
    ) -> anyhow::Result<()> {
        let benchmark = Benchmark::from_toml(
            "vlm_eval_mmstar",
            &format!(
                "benchmark_type = \"vlm_eval\"\n\
                 parameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"2026.10.1\"\n\
                 parameter_sample_count = 1500\n\
                 parameter_max_tokens = 8192\n{pinned}\n{}",
                pipette_core::dataset::vlm_eval_catalog_tables("mmstar", "2026.10.1", None)
            ),
        )?;
        let rows = failure_rows(
            benchmark,
            json!({
                "benchmark_type": "vlm_eval",
                "retriable": true,
                "failure_reason": "llama-server exited mid-eval",
            }),
            None,
        )?;

        assert!(!rows.is_empty());
        rows.iter().for_each(|r| {
            assert_eq!(
                [
                    r.parameter_temperature,
                    r.parameter_top_p,
                    r.parameter_top_k.map(f64::from),
                    r.parameter_min_p,
                    r.parameter_presence_penalty,
                    r.parameter_repeat_penalty,
                ],
                knobs,
                "row {}",
                r.result_id
            );
            assert_eq!(r.parameter_seed, seed, "row {}", r.result_id);
            assert_eq!(r.benchmark_params.as_deref(), Some(params));
        });
        Ok(())
    }

    /// A vlm_eval stamps the prompt profile it names, which its descriptor
    /// carries too; one that names none keeps the descriptor it had before.
    #[rstest]
    #[case::dataset_prompt(
        "",
        None,
        r#"{"benchmark_type":"vlm_eval","dataset_version":"2026.10.1","lmms_task":"mmstar","max_tokens":8192,"sample_count":1500}"#
    )]
    #[case::profiled(
        "parameter_prompt_profile = \"minicpm_v_cot\"",
        Some("minicpm_v_cot"),
        r#"{"benchmark_type":"vlm_eval","dataset_version":"2026.10.1","lmms_task":"mmstar","max_tokens":8192,"prompt_profile":"minicpm_v_cot","sample_count":1500}"#
    )]
    fn test_vlm_eval_failure_rows_carry_the_prompt_profile(
        #[case] named: &str,
        #[case] profile: Option<&str>,
        #[case] params: &str,
    ) -> anyhow::Result<()> {
        let benchmark = Benchmark::from_toml(
            "vlm_eval_mmstar",
            &format!(
                "benchmark_type = \"vlm_eval\"\n\
                 parameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"2026.10.1\"\n\
                 parameter_sample_count = 1500\n\
                 parameter_max_tokens = 8192\n{named}\n{}",
                pipette_core::dataset::vlm_eval_catalog_tables("mmstar", "2026.10.1", profile)
            ),
        )?;
        let rows = failure_rows(
            benchmark,
            json!({
                "benchmark_type": "vlm_eval",
                "retriable": true,
                "failure_reason": "llama-server exited mid-eval",
            }),
            None,
        )?;

        assert!(!rows.is_empty());
        rows.iter().for_each(|r| {
            assert_eq!(
                r.parameter_prompt_profile.as_deref(),
                profile,
                "row {}",
                r.result_id
            );
            assert_eq!(r.benchmark_params.as_deref(), Some(params));
        });
        Ok(())
    }

    /// An entry's `org_id` stays out of the descriptor.
    #[rstest]
    #[case::public(None)]
    #[case::private(Some("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"))]
    #[tokio::test]
    async fn test_benchmark_params_reach_measured_and_failure_rows(
        #[case] org_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let params =
            r#"{"batch_size":4,"benchmark_type":"prefill_throughput","prefill_tokens":256}"#;
        let benchmark = Benchmark {
            org_id: org_id
                .map(|id| pipette_plan_types::OrgId::try_new(id.to_owned()))
                .transpose()?,
            def: BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new("prefill_throughput_256_bs4")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
                parameter_batch_size: BatchSize::try_new(4)?,
            }),
        };
        let failed = failure_rows(
            benchmark.clone(),
            json!({
                "benchmark_type": "prefill_throughput",
                "retriable": false,
                "failure_reason": "OOM",
            }),
            None,
        )?;
        let measured = score_one(benchmark, json!({ "prefill_time_ms": 34.7 })).await?;

        assert!(!measured.is_empty() && !failed.is_empty());
        let sha256 = pipette_core::canonical_json::sha256_hex(params);
        let off: Vec<_> = measured
            .iter()
            .chain(&failed)
            .filter(|r| {
                r.benchmark_params.as_deref() != Some(params)
                    || r.benchmark_params_sha256.as_deref() != Some(sha256.as_str())
            })
            .map(|r| r.result_id.as_str())
            .collect();
        assert!(off.is_empty(), "rows without the descriptor: {off:?}");
        Ok(())
    }

    /// A VL entry's counts stay integers, its empty prompt included. An entry
    /// without a decode count keeps its params and a NULL decode column.
    #[rstest]
    #[case::legacy(
        "",
        r#"{"benchmark_type":"vl_max_memory","image_height":256,"image_width":256,"num_images":1,"text_tokens":0}"#,
        None
    )]
    #[case::decode(
        "\nparameter_decode_tokens = 256",
        r#"{"benchmark_type":"vl_max_memory","decode_tokens":256,"image_height":256,"image_width":256,"num_images":1,"text_tokens":0}"#,
        Some(256)
    )]
    fn test_vl_rows_carry_integer_counts(
        #[case] decode_line: &str,
        #[case] params: &str,
        #[case] decode_tokens: Option<i32>,
    ) -> anyhow::Result<()> {
        let benchmark = Benchmark::from_toml(
            "vl_max_memory_256x256_text0_img1",
            &format!(
                "benchmark_type = \"vl_max_memory\"\n\
                 parameter_image_width = 256\n\
                 parameter_image_height = 256{decode_line}"
            ),
        )?;
        let rows = failure_rows(
            benchmark,
            json!({
                "benchmark_type": "vl_max_memory",
                "retriable": false,
                "failure_reason": "OOM",
            }),
            None,
        )?;

        assert!(!rows.is_empty());
        let off: Vec<_> = rows
            .iter()
            .filter(|r| {
                r.benchmark_params.as_deref() != Some(params)
                    || r.parameter_decode_tokens != decode_tokens
            })
            .map(|r| r.result_id.as_str())
            .collect();
        assert!(off.is_empty(), "rows without the pair: {off:?}");
        Ok(())
    }

    /// The rows [`record_failure`] writes for one failure of `benchmark`;
    /// `fields` joins the failure body.
    fn failure_rows(
        benchmark: Benchmark,
        fields: Value,
        alias: Option<&str>,
    ) -> anyhow::Result<Vec<MetricRow>> {
        let mut body = json!({
            "benchmark_id": benchmark.benchmark_id().as_str(),
            "client_id": "test",
            "job_id": "test",
            "submitted_at": "2026-01-01T00:00:00Z",
        });
        body.as_object_mut()
            .context("the failure body is a JSON object")?
            .extend(
                fields
                    .as_object()
                    .context("`fields` is a JSON object")?
                    .clone(),
            );
        let failure: FailureSubmission = serde_json::from_value(body)?;
        let catalog = HashMap::from([(benchmark.benchmark_id().clone(), benchmark)]);
        let model_table = crate::model_params::ModelParamTable::default();
        let models = PassTable::new(&model_table);
        let names = VaultNames {
            alias: alias.map(str::to_string),
            ..VaultNames::default()
        };
        Ok(record_failure(&catalog, &models, failure, &names)?.metric_rows)
    }

    #[tokio::test]
    async fn test_client_extras_reach_measured_and_failure_rows() -> anyhow::Result<()> {
        let extras = r#"{ "build": "nightly" }"#;
        let benchmark = prefill_benchmark()?;
        let failed = failure_rows(
            benchmark.clone(),
            json!({
                "benchmark_type": "prefill_throughput",
                "retriable": false,
                "failure_reason": "OOM",
                "client_extras": extras,
            }),
            None,
        )?;
        let measured = score_one(
            benchmark,
            json!({ "prefill_time_ms": 34.7, "client_extras": extras }),
        )
        .await?;

        assert!(!measured.is_empty() && !failed.is_empty());
        let missing: Vec<&str> = measured
            .iter()
            .chain(&failed)
            .filter(|r| r.client_extras.as_deref() != Some(extras))
            .map(|r| r.result_id.as_str())
            .collect();
        assert!(missing.is_empty(), "rows without the extras: {missing:?}");
        Ok(())
    }

    #[tokio::test]
    async fn test_device_time_ms_reaches_measured_and_failure_rows() -> anyhow::Result<()> {
        let benchmark = prefill_benchmark()?;
        let failed = failure_rows(
            benchmark.clone(),
            json!({
                "benchmark_type": "prefill_throughput",
                "retriable": false,
                "failure_reason": "OOM",
                "device_time_ms": 9_500,
            }),
            None,
        )?;
        let measured = score_one(
            benchmark,
            json!({ "prefill_time_ms": 34.7, "device_time_ms": 184_250 }),
        )
        .await?;

        assert!(!measured.is_empty() && !failed.is_empty());
        assert!(measured.iter().all(|r| r.device_time_ms == Some(184_250)));
        assert!(failed.iter().all(|r| r.device_time_ms == Some(9_500)));
        assert!(
            failed
                .iter()
                .all(|r| r.run_submitted_at == Some(r.submitted_at))
        );
        Ok(())
    }

    #[test]
    fn test_a_failure_row_records_the_alias() -> anyhow::Result<()> {
        let benchmark = prefill_benchmark()?;
        let reported = r#"{"type":"gguf_text","source":"vault_url","url":"/vault/resources/vault_1/content","principal_id":"org_1"}"#;
        let alias = r#"{"type":"gguf_text","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-GGUF","path":"m.gguf"}"#;
        let rows = failure_rows(
            benchmark,
            json!({
                "benchmark_type": "prefill_throughput",
                "retriable": false,
                "failure_reason": "OOM",
                "model_descriptor": reported,
            }),
            Some(alias),
        )?;
        let alias = pipette_core::canonical_json::canonicalize_str(alias);
        let reported = pipette_core::canonical_json::canonicalize_str(reported);
        assert!(!rows.is_empty());
        rows.iter().for_each(|r| {
            assert_eq!(r.model_descriptor.as_deref(), Some(alias.as_str()));
            assert_eq!(
                r.model_descriptor_sha256,
                Some(pipette_core::canonical_json::sha256_hex(&alias))
            );
            assert_eq!(r.model_vault_descriptor.as_deref(), Some(reported.as_str()));
        });
        Ok(())
    }

    #[rstest]
    #[case::copy(
        Some("job-a"),
        Some("2025-12-01T00:00:00Z"),
        Some("2025-12-01T00:00:00Z")
    )]
    #[case::copy_without_run_time(Some("job-a"), None, None)]
    #[case::device(None, None, Some("2026-01-01T00:00:00Z"))]
    #[tokio::test]
    async fn test_rows_carry_the_job_they_rescore(
        #[case] source: Option<&str>,
        #[case] run_submitted_at: Option<&str>,
        #[case] expected_run: Option<&str>,
    ) -> anyhow::Result<()> {
        let benchmark = prefill_benchmark()?;

        let rows = score_one(
            benchmark,
            json!({
                "prefill_time_ms": 34.7,
                "rescored_from": source,
                "run_submitted_at": run_submitted_at,
            }),
        )
        .await?;

        assert!(!rows.is_empty());
        let expected_run = expected_run
            .map(|at| {
                at.parse::<chrono::DateTime<Utc>>()
                    .map(|at| at.timestamp_micros())
            })
            .transpose()?;
        rows.iter().for_each(|r| {
            assert_eq!(r.rescored_from.as_deref(), source);
            assert_eq!(r.run_submitted_at, expected_run, "{}", r.result_id);
        });
        // A copy's rows carry the entry's pair, like a device's.
        let params =
            r#"{"batch_size":1,"benchmark_type":"prefill_throughput","prefill_tokens":256}"#;
        let off: Vec<_> = rows
            .iter()
            .filter(|r| r.benchmark_params.as_deref() != Some(params))
            .map(|r| r.result_id.as_str())
            .collect();
        assert!(off.is_empty(), "rows without the pair: {off:?}");
        Ok(())
    }

    #[tokio::test]
    async fn test_non_eval_rows_carry_no_sampling() -> anyhow::Result<()> {
        let benchmark = prefill_benchmark()?;

        let rows = score_one(benchmark, json!({ "prefill_time_ms": 34.7 })).await?;

        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| {
            r.parameter_temperature.is_none()
                && r.parameter_top_p.is_none()
                && r.parameter_top_k.is_none()
                && r.parameter_min_p.is_none()
                && r.parameter_seed.is_none()
        }));
        Ok(())
    }

    /// A batched run must never pair with the bs=1 reference.
    #[rstest]
    #[case::prefill(
        "prefill_throughput_256",
        |benchmark_id, bs| Ok(BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            benchmark_id,
            parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
            parameter_batch_size: bs,
        })),
        json!({ "prefill_time_ms": 34.7 }),
    )]
    #[case::decode(
        "decode_throughput_512_100",
        |benchmark_id, bs| Ok(BenchmarkDefinition::DecodeThroughput(DecodeThroughputBenchmark {
            benchmark_id,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
            parameter_decode_tokens: PositiveCount::try_new(100)?,
            parameter_batch_size: bs,
        })),
        json!({ "decode_time_ms": 500.0 }),
    )]
    #[case::end_to_end(
        "end_to_end_latency_256_256",
        |benchmark_id, bs| Ok(BenchmarkDefinition::EndToEndLatency(EndToEndLatencyBenchmark {
            benchmark_id,
            parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
            parameter_decode_tokens: PositiveCount::try_new(256)?,
            parameter_batch_size: bs,
        })),
        json!({ "total_time_ms": 123.4 }),
    )]
    #[tokio::test]
    async fn test_stamps_batch_size(
        #[case] id: &str,
        #[case] def: fn(BenchmarkId, BatchSize) -> anyhow::Result<BenchmarkDefinition>,
        #[case] body: serde_json::Value,
        #[values(1, 4)] batch_size: u32,
    ) -> anyhow::Result<()> {
        let benchmark = Benchmark {
            org_id: None,
            def: def(BenchmarkId::try_new(id)?, BatchSize::try_new(batch_size)?)?,
        };

        let rows = score_one(benchmark, body).await?;

        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|r| r.parameter_batch_size == i32::try_from(batch_size).ok())
        );
        Ok(())
    }

    /// The gate is the benchmark definition, never the presence of the field.
    #[tokio::test]
    async fn test_corpus_fields_are_ignored_off_a_corpus_benchmark() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new("vl_throughput_512x512_text32_decode100_img1")?,
                parameter_image_width: PositiveCount::try_new(512)?,
                parameter_image_height: PositiveCount::try_new(512)?,
                parameter_text_tokens: NonNegativeCount::try_new(32)?,
                parameter_decode_tokens: PositiveCount::try_new(100)?,
                parameter_num_images: NonNegativeCount::try_new(1)?,
            }),
        };
        let wire = json!({
            "prompt_tokens": 300,
            "prompt_ms": 120.0,
            "predicted_ms": 400.0,
            // A client bug, or a shared harness struct serialized wholesale.
            "runtime_flags": r#"{"speculative_enabled":false}"#,
            "vl_spec_decode_turns": 723,
            "observation_output_digest": "3".repeat(64),
            "observation_dataset_digest": FIXTURE_DIGEST,
        });

        let rows = score_one(benchmark, wire).await?;

        anyhow::ensure!(!rows.is_empty(), "scoring produced no rows");
        rows.iter().for_each(|row| {
            assert_eq!(row.runtime_speculative_enabled, None);
            assert_eq!(row.observation_vl_spec_decode_turns, None);
            assert_eq!(row.observation_output_digest, None);
            // No dataset, so nothing to compare the digest against.
            assert_eq!(row.observation_dataset_digest_matched, None);
        });
        Ok(())
    }

    /// The drafter's three only when the client resolved speculation on before
    /// failing; table parameters still filled so the failure groups.
    #[rstest]
    #[case::resolved_on(json!(r#"{"speculative_enabled":true}"#), Some(true), 9)]
    #[case::resolved_off(json!(r#"{"speculative_enabled":false}"#), Some(false), 6)]
    #[case::unresolved(serde_json::Value::Null, None, 6)]
    fn test_vl_spec_decode_failure_rows_carry_the_dataset(
        #[case] runtime_flags: Value,
        #[case] speculative: Option<bool>,
        #[case] metrics: usize,
    ) -> anyhow::Result<()> {
        let rows = failure_rows(
            vl_spec_decode_benchmark()?,
            json!({
                "benchmark_type": "vl_spec_decode",
                "retriable": true,
                "failure_reason": "llama-server exited mid-traversal",
                "runtime_flags": runtime_flags,
            }),
            None,
        )?;

        assert_eq!(rows.len(), metrics);
        assert!(rows.iter().all(|r| {
            r.failed == Some(true)
                && r.value.is_none()
                && r.parameter_draft_length == Some(8)
                && r.parameter_batch_size == Some(1)
                && r.parameter_temperature == Some(0.0)
                && r.parameter_dataset_subset.as_deref() == Some("GQA")
                && r.runtime_speculative_enabled == speculative
        }));
        Ok(())
    }

    /// One-directional: `derive_metrics` may emit more than the intended set.
    #[rstest]
    #[case::prefill(
        BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
            parameter_batch_size: BatchSize::one(),
        }),
        json!({"prefill_time_ms": 34.7}),
    )]
    #[case::decode(
        BenchmarkDefinition::DecodeThroughput(DecodeThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
            parameter_decode_tokens: PositiveCount::try_new(100)?,
            parameter_batch_size: BatchSize::one(),
        }),
        json!({"decode_time_ms": 500.0}),
    )]
    #[case::end_to_end(
        BenchmarkDefinition::EndToEndLatency(EndToEndLatencyBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
            parameter_decode_tokens: PositiveCount::try_new(100)?,
            parameter_batch_size: BatchSize::one(),
        }),
        json!({"total_time_ms": 900.0}),
    )]
    #[case::max_memory(
        BenchmarkDefinition::MaxMemoryUsage(MaxMemoryUsageBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_prefill_tokens: NonNegativeCount::try_new(256)? }),
        json!({"max_host_bytes": 1_000_000_000_i64, "max_gpu_bytes": 500_000_000_i64}),
    )]
    #[case::vl_max_memory(
        BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_image_width: PositiveCount::try_new(512)?,
            parameter_image_height: PositiveCount::try_new(512)?,
            parameter_text_tokens: NonNegativeCount::try_new(32)?,
            parameter_decode_tokens: None,
            parameter_num_images: NonNegativeCount::try_new(1)?,
        }),
        json!({"max_host_bytes": 1_000_000_000_i64}),
    )]
    #[case::asr_max_memory(
        BenchmarkDefinition::AsrMaxMemory(AsrMaxMemoryBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_audio_duration_ms: AudioDurationMillis::try_new(9400)?,
            parameter_audio_sample_rate_hz: AudioSampleRate::try_new(16_000)?,
            parameter_audio_channels: AudioChannels::try_new(1)?,
            parameter_text_tokens: NonNegativeCount::try_new(1024)?,
            parameter_decode_tokens: AsrDecodeTokens::try_new(256)?,
        }),
        json!({"max_host_bytes": 1_000_000_000_i64}),
    )]
    #[case::eval(
        BenchmarkDefinition::Eval(EvalBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_eval_id: "e1".into(),
            parameter_dataset_name: "d1".to_string(),
            parameter_max_tokens: PositiveCount::try_new(128)?,
            parameter_max_context: None,
            parameter_mcq_choices: None,
            parameter_temperature: None,
            parameter_top_p: None,
            parameter_top_k: None,
            parameter_min_p: None,
            parameter_seed: None,
            samples: None,
        }),
        json!({"completions": [{"id": "s1", "completion": "a"}]}),
    )]
    #[case::vl_throughput(
        BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_image_width: PositiveCount::try_new(512)?,
            parameter_image_height: PositiveCount::try_new(512)?,
            parameter_text_tokens: NonNegativeCount::try_new(32)?,
            parameter_decode_tokens: PositiveCount::try_new(100)?,
            parameter_num_images: NonNegativeCount::try_new(1)?,
        }),
        json!({"prompt_tokens": 300, "prompt_ms": 120.0, "predicted_ms": 400.0}),
    )]
    #[case::asr_throughput(
        BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_audio_duration_ms: AudioDurationMillis::try_new(9400)?,
            parameter_audio_sample_rate_hz: AudioSampleRate::try_new(16_000)?,
            parameter_audio_channels: AudioChannels::try_new(1)?,
            parameter_text_tokens: NonNegativeCount::try_new(0)?,
            parameter_decode_tokens: AsrDecodeTokens::try_new(128)?,
        }),
        json!({"asr_prompt_tokens": 137, "asr_audio_tokens": 118, "asr_prompt_ms": 83.3, "asr_predicted_ms": 129.4}),
    )]
    #[case::vl_spec_decode_off(vl_spec_decode_benchmark()?.def, vl_spec_decode_wire(false))]
    #[case::vl_spec_decode_on(vl_spec_decode_benchmark()?.def, vl_spec_decode_wire(true))]
    #[case::asr_eval_wer(
        BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
            parameter_sample_count: PositiveCount::try_new(1)?,
            ..asr_eval_def("asr_esb_librispeech_test_clean")?
        }),
        json!({"completions": [{"id": "s1", "completion": "a"}], "observation_dataset_digest": "1".repeat(64)}),
    )]
    #[case::asr_eval_fleurs(
        BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
            parameter_sample_count: PositiveCount::try_new(1)?,
            ..asr_eval_def("asr_fleurs_de_de")?
        }),
        json!({"completions": [{"id": "s1", "completion": "a"}], "observation_dataset_digest": "1".repeat(64)}),
    )]
    #[case::asr_eval_dropped_locale(
        BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
            parameter_sample_count: PositiveCount::try_new(1)?,
            ..asr_eval_def("asr_fleurs_ja_jp")?
        }),
        json!({"completions": [{"id": "s1", "completion": "a"}], "observation_dataset_digest": "1".repeat(64)}),
    )]
    #[case::asr_eval_unknown_task(
        BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
            parameter_sample_count: PositiveCount::try_new(1)?,
            ..asr_eval_def("asr_fleurs_th_th")?
        }),
        json!({"completions": [{"id": "s1", "completion": "a"}], "observation_dataset_digest": "1".repeat(64)}),
    )]
    #[tokio::test]
    async fn test_intended_metrics_agrees_with_derive_metrics(
        #[case] def: BenchmarkDefinition,
        #[case] extras: Value,
    ) -> anyhow::Result<()> {
        let benchmark = Benchmark { org_id: None, def };
        let submission = test_submission(extras)?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            ..Config::default()
        };
        // Canned, so the eval cases derive without a network call; an asr_eval
        // context counts words, as the scorer does for every task.
        let context = match benchmark.def.as_asr_eval() {
            Ok(_) => asr_context("word", 0.0),
            Err(_) => std::collections::BTreeMap::new(),
        };
        let eval_response = Some(crate::scoring_service::ScoreResponse {
            runtime_version: "1.0.0".to_string(),
            scored_samples: vec![crate::scoring_service::ScoredSample {
                id: pipette_plan_types::NonEmptyTrimmedString::try_new("s1")?,
                messages: Vec::new(),
                completion: "a".to_string(),
                is_correct: true,
                measures: None,
                chat_template_kwargs: None,
            }],
            context,
        });
        let derived = derive_metrics(
            &benchmark,
            &submission,
            &config,
            &test_client()?,
            eval_response,
        )
        .await?;

        let intended = intended_metrics_of(
            &benchmark.def,
            resolved_speculation(
                &benchmark.def,
                submission.wire.runtime_flags.as_ref().map(AsRef::as_ref),
            ),
        );
        assert!(
            !intended.is_empty(),
            "every type intends at least one metric"
        );
        for (metric, unit) in intended {
            let derived_metric = derived
                .metrics
                .iter()
                .find(|d| d.metric == *metric)
                .with_context(|| format!("derive_metrics did not produce {metric}"))?;
            assert_eq!(derived_metric.unit, *unit, "unit disagrees for {metric}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_derive_prefill_metrics() -> anyhow::Result<()> {
        let benchmark = prefill_benchmark()?;
        let submission = test_submission(json!({
            "prefill_time_ms": 34.7,
            "prefill_time_ms_stddev": 1.25
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
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
            org_id: None,
            def: BenchmarkDefinition::DecodeThroughput(DecodeThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new("decode_throughput_512_100")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
                parameter_decode_tokens: PositiveCount::try_new(100)?,
                parameter_batch_size: BatchSize::one(),
            }),
        };
        let submission = test_submission(json!({
            "decode_time_ms": 50.0,
            "decode_time_ms_stddev": 2.0
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
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
            org_id: None,
            def: BenchmarkDefinition::EndToEndLatency(EndToEndLatencyBenchmark {
                benchmark_id: BenchmarkId::try_new("end_to_end_latency_256_256")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
                parameter_decode_tokens: PositiveCount::try_new(256)?,
                parameter_batch_size: BatchSize::one(),
            }),
        };
        let submission = test_submission(json!({ "total_time_ms": 123.4 }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
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
            org_id: None,
            def: BenchmarkDefinition::MaxMemoryUsage(MaxMemoryUsageBenchmark {
                benchmark_id: BenchmarkId::try_new("max_memory_usage_256")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
            }),
        };
        let submission = test_submission(json!({
            "max_host_bytes": 1073741824_i64,
            "max_gpu_bytes": 536870912_i64,
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
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
            org_id: None,
            def: BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
                benchmark_id: BenchmarkId::try_new("vl_max_memory_512x512_t0_f1")?,
                parameter_image_width: PositiveCount::try_new(512)?,
                parameter_image_height: PositiveCount::try_new(512)?,
                parameter_text_tokens: NonNegativeCount::try_new(0)?,
                parameter_decode_tokens: None,
                parameter_num_images: NonNegativeCount::try_new(1)?,
            }),
        };
        let submission = test_submission(json!({
            "max_host_bytes": 2147483648_i64,
            "max_gpu_bytes": 1073741824_i64,
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].metric, "max_host_usage");
        assert!((metrics[0].value - 2_f32.powi(31)).abs() < 1.0);
        assert_eq!(metrics[1].metric, "max_gpu_usage");
        assert!((metrics[1].value - 2_f32.powi(30)).abs() < 1.0);
        Ok(())
    }

    #[tokio::test]
    async fn test_max_memory_usage_without_gpu() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::MaxMemoryUsage(MaxMemoryUsageBenchmark {
                benchmark_id: BenchmarkId::try_new("max_memory_usage_256")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
            }),
        };
        let submission = test_submission(json!({
            "max_host_bytes": 1073741824_i64,
            "max_gpu_bytes": null,
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
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
            org_id: None,
            def: BenchmarkDefinition::MaxMemoryUsage(MaxMemoryUsageBenchmark {
                benchmark_id: BenchmarkId::try_new("max_memory_usage_256")?,
                parameter_prefill_tokens: NonNegativeCount::try_new(256)?,
            }),
        };
        let submission = test_submission(json!({
            "max_host_bytes": 1073741824_i64,
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].metric, "max_host_usage");
        assert_eq!(metrics[0].value_stddev, None);
        Ok(())
    }

    /// VL and ASR throughput derive the same three metrics from the same counts
    /// and timings, the ASR ones under their `asr_` names, so the two read side
    /// by side; token counts are observation columns, not metrics.
    #[rstest]
    #[case::vl(false)]
    #[case::asr(true)]
    #[tokio::test]
    async fn test_derive_throughput_metrics(#[case] asr: bool) -> anyhow::Result<()> {
        let (benchmark, body) = throughput_fixture(asr)?;
        let submission = test_submission(body)?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
        let derived = derive_metrics(&benchmark, &submission, &config, &client, None).await?;
        assert!(derived.eval_sample_results.is_none());
        let metrics = &derived.metrics;
        assert_eq!(metrics.len(), 3);
        assert_eq!(
            intended_metrics(benchmark.def.benchmark_type(), None).len(),
            3
        );

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
        Ok(())
    }

    fn vl_throughput_384x512() -> anyhow::Result<Benchmark> {
        Ok(Benchmark {
            org_id: None,
            def: BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new("vl_throughput_384x512_32_128")?,
                parameter_image_width: PositiveCount::try_new(384)?,
                parameter_image_height: PositiveCount::try_new(512)?,
                parameter_text_tokens: NonNegativeCount::try_new(32)?,
                parameter_decode_tokens: PositiveCount::try_new(128)?,
                parameter_num_images: NonNegativeCount::try_new(1)?,
            }),
        })
    }

    fn asr_throughput_benchmark() -> anyhow::Result<Benchmark> {
        Ok(Benchmark {
            org_id: None,
            def: BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new("asr_throughput_9400ms_text0_decode128")?,
                parameter_audio_duration_ms: AudioDurationMillis::try_new(9_400)?,
                parameter_audio_sample_rate_hz: AudioSampleRate::try_new(16_000)?,
                parameter_audio_channels: AudioChannels::try_new(1)?,
                parameter_text_tokens: NonNegativeCount::try_new(0)?,
                parameter_decode_tokens: AsrDecodeTokens::try_new(128)?,
            }),
        })
    }

    /// A VL or ASR throughput benchmark decoding 128 tokens and a body with the
    /// same counts and timings in that type's field names.
    fn throughput_fixture(asr: bool) -> anyhow::Result<(Benchmark, Value)> {
        Ok(if asr {
            (
                asr_throughput_benchmark()?,
                json!({
                    "asr_prompt_tokens": 224,
                    "asr_audio_tokens": 192,
                    "asr_prompt_ms": 50.0,
                    "asr_prompt_ms_stddev": 2.5,
                    "asr_predicted_ms": 200.0,
                    "asr_predicted_ms_stddev": 10.0
                }),
            )
        } else {
            (
                vl_throughput_384x512()?,
                json!({
                    "prompt_tokens": 224,
                    "image_tokens": 192,
                    "prompt_ms": 50.0,
                    "prompt_ms_stddev": 2.5,
                    "predicted_ms": 200.0,
                    "predicted_ms_stddev": 10.0
                }),
            )
        })
    }

    #[rstest]
    #[case::none_without_a_total(json!({}), None, None)]
    #[case::the_total_as_sent(json!({"total_time_ms": 262.5, "total_time_ms_stddev": 3.0}), Some(262.5), Some(3.0))]
    #[case::no_spread_from_one_rep(json!({"total_time_ms": 262.5}), Some(262.5), None)]
    #[case::a_lone_total_stddev_is_ignored(json!({"total_time_ms_stddev": 3.0}), None, None)]
    #[tokio::test]
    async fn test_derive_throughput_end_to_end_latency(
        #[case] extra: Value,
        #[case] value: Option<f32>,
        #[case] stddev: Option<f32>,
        #[values(false, true)] asr: bool,
    ) -> anyhow::Result<()> {
        let (benchmark, mut wire) = throughput_fixture(asr)?;
        let Value::Object(map) = extra else {
            anyhow::bail!("a case's extra must be a JSON object")
        };
        wire.as_object_mut()
            .context("the fixture body is a JSON object")?
            .extend(map);
        let submission = test_submission(wire)?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            ..Config::default()
        };
        let derived =
            derive_metrics(&benchmark, &submission, &config, &test_client()?, None).await?;
        let metrics = &derived.metrics;

        assert_eq!(metrics.len(), 3 + usize::from(value.is_some()));
        assert!(metrics.iter().all(|m| m.metric != "e2e_latency"));

        let end_to_end = metrics.iter().find(|m| m.metric == "end_to_end_latency");
        assert_eq!(end_to_end.map(|m| m.unit.as_str()), value.map(|_| "ms"));
        let near = |got: Option<f32>, want: Option<f32>| match (got, want) {
            (Some(g), Some(w)) => (g - w).abs() < 0.01,
            (g, w) => g == w,
        };
        let got_value = end_to_end.map(|m| m.value);
        assert!(near(got_value, value), "got {got_value:?}");
        let got_stddev = end_to_end.and_then(|m| m.value_stddev);
        assert!(near(got_stddev, stddev), "got {got_stddev:?}");
        Ok(())
    }

    /// A body without an ASR measurement cannot be scored: the derivation names
    /// the missing field rather than producing a metric from nothing.
    #[tokio::test]
    async fn test_derive_asr_throughput_requires_its_measurements() -> anyhow::Result<()> {
        let benchmark = asr_throughput_benchmark()?;
        let submission = test_submission(json!({
            "asr_prompt_tokens": 224,
            "asr_audio_tokens": 192,
            "asr_predicted_ms": 200.0
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
        let err = derive_metrics(&benchmark, &submission, &config, &client, None)
            .await
            .err()
            .context("a body without asr_prompt_ms was scored")?;
        assert!(err.to_string().contains("asr_prompt_ms"), "{err:#}");
        Ok(())
    }

    // ---- build_eval_metadata -------------------------------------------

    #[tokio::test]
    async fn derive_metrics_bails_on_empty_eval_completions() -> anyhow::Result<()> {
        // Must bail before the /score call: a regression shows as a network
        // error instead of the message asserted below.
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::Eval(EvalBenchmark {
                benchmark_id: BenchmarkId::try_new("eval_bench")?,
                parameter_eval_id: "test_eval".into(),
                parameter_dataset_name: "default".to_string(),
                parameter_max_tokens: PositiveCount::try_new(100)?,
                parameter_max_context: None,
                parameter_mcq_choices: None,
                parameter_temperature: None,
                parameter_top_p: None,
                parameter_top_k: None,
                parameter_min_p: None,
                parameter_seed: None,
                samples: None,
            }),
        };
        let submission = test_submission(json!({
            "benchmark_id": "eval_bench",
            "benchmark_type": "eval",
            "completions": [],
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            ..Config::default()
        };
        let client = test_client()?;
        let result = derive_metrics(&benchmark, &submission, &config, &client, None).await;
        let Err(err) = result else {
            anyhow::bail!("expected bail on empty completions")
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("no completions"),
            "unexpected error message: {msg}"
        );
        Ok(())
    }

    fn asr_eval_def(task: &str) -> anyhow::Result<AsrEvalBenchmark> {
        Ok(AsrEvalBenchmark {
            benchmark_id: BenchmarkId::try_new("drift-guard")?,
            parameter_asr_task: task.into(),
            parameter_dataset_version: "2026.10.1".to_string(),
            parameter_sample_count: PositiveCount::try_new(2)?,
            parameter_prompt_profile: "qwen25_omni".into(),
            parameter_max_tokens: PositiveCount::try_new(256)?,
            parameter_temperature: None,
            parameter_top_p: None,
            parameter_top_k: None,
            parameter_min_p: None,
            parameter_repeat_penalty: None,
            parameter_seed: None,
            parameter_seed_rule: None,
            parameter_num_beams: None,
            parameter_beam_repetition_penalty: None,
            parameter_beam_length_penalty: None,
            dataset: Some(Box::new(serde_json::from_value(json!({
                "format": "asr-eval-corpus/1",
                "adapter": "evals-server@1",
                "digest": "1".repeat(64),
                "bytes": 1024,
                "upstream": {
                    "source": "evals_server",
                    "eval_id": task,
                    "dataset_version": "2026.10.1",
                    "prompt_profile": "qwen25_omni",
                    "identity": "2".repeat(64),
                },
            }))?)),
            recipe: Some(serde_json::from_value(json!({
                "family": "Test family",
                "models": [{"repo": "org/repo"}],
                "enable_thinking": false,
                "sources": ["https://huggingface.co/org/repo"],
            }))?),
        })
    }

    /// A complete run's context, as the scorer sends it.
    fn asr_context(unit: &str, error_rate: f64) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("error_unit".to_string(), json!(unit)),
            ("error_rate".to_string(), json!(error_rate)),
            ("result".to_string(), json!(error_rate * 100.0)),
            ("samples_total".to_string(), json!(2)),
            ("samples_excluded".to_string(), json!(0)),
        ])
    }

    /// Two scored clips, the first with its audio still in its messages.
    fn asr_response(
        context: BTreeMap<String, Value>,
    ) -> anyhow::Result<scoring_service::ScoreResponse> {
        let messages: Vec<pipette_plan_types::ChatMessage> = serde_json::from_value(json!([
            {"role": "system", "content": "Transcribe."},
            {"role": "user", "content": [
                {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}},
                {"type": "text", "text": "the clip"},
            ]},
        ]))?;
        let measures = json!({"unit": "s1", "hits": 2, "substitutions": 1, "deletions": 0,
            "insertions": 0, "reference_units": 3, "reference": "a b c", "hypothesis": "a x c"});
        Ok(scoring_service::ScoreResponse {
            runtime_version: "1.0.0".to_string(),
            scored_samples: vec![
                scoring_service::ScoredSample {
                    id: pipette_plan_types::NonEmptyTrimmedString::try_new("s1")?,
                    messages,
                    completion: "a x c".to_string(),
                    is_correct: false,
                    measures: Some(measures),
                    chat_template_kwargs: None,
                },
                scoring_service::ScoredSample {
                    id: pipette_plan_types::NonEmptyTrimmedString::try_new("s2")?,
                    messages: Vec::new(),
                    completion: "d".to_string(),
                    is_correct: true,
                    measures: None,
                    chat_template_kwargs: None,
                },
            ],
            context,
        })
    }

    async fn derive_asr(
        task: &str,
        response: scoring_service::ScoreResponse,
    ) -> anyhow::Result<DerivedResult> {
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::AsrEval(asr_eval_def(task)?),
        };
        let submission = test_submission(json!({
            "benchmark_id": "drift-guard",
            "benchmark_type": "asr_eval",
            "observation_dataset_digest": "1".repeat(64),
            "completions": [{"id": "s1", "completion": "a x c"}, {"id": "s2", "completion": "d"}],
        }))?;
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            ..Config::default()
        };
        derive_metrics(
            &benchmark,
            &submission,
            &config,
            &test_client()?,
            Some(response),
        )
        .await
    }

    /// The headline is the scorer's corpus rate, not a per-sample mean, and
    /// each row keeps its measures but drops its clip.
    #[tokio::test]
    async fn asr_eval_reports_the_corpus_error_rate() -> anyhow::Result<()> {
        let derived = derive_asr(
            "asr_esb_librispeech_test_clean",
            asr_response(asr_context("word", 0.25))?,
        )
        .await?;
        assert_eq!(derived.metrics.len(), 1);
        assert_eq!(derived.metrics[0].metric, "wer");
        assert_eq!(derived.metrics[0].unit, "ratio");
        assert!((derived.metrics[0].value - 0.25).abs() < 1e-6);
        let rows = derived.eval_sample_results.context("asr rows")?;
        let measures: Value =
            serde_json::from_str(rows[0].measures.as_deref().context("s1 measures")?)?;
        assert_eq!(measures["substitutions"], 1);
        assert_eq!(rows[1].measures, None);
        // The clip is not stored, but its part is, empty, so the turn's
        // order stays auditable.
        assert!(
            !rows[0].messages.contains("UklGRg=="),
            "{}",
            rows[0].messages
        );
        assert!(
            rows[0]
                .messages
                .contains(r#""input_audio":{"data":"","format":"wav"}"#),
            "{}",
            rows[0].messages
        );
        assert!(
            rows[0].messages.contains("the clip"),
            "{}",
            rows[0].messages
        );
        Ok(())
    }

    /// A sample's chat-template arguments, as `/score` returns them, reach
    /// its stored row and read back from the Parquet unchanged; a sample
    /// served without any stores `null`, as a vlm_eval's does.
    #[tokio::test]
    async fn asr_eval_rows_keep_their_chat_template_kwargs() -> anyhow::Result<()> {
        let mut body = serde_json::to_value(asr_response(asr_context("word", 0.25))?)?;
        assert!(
            body["scored_samples"][1]
                .get("chat_template_kwargs")
                .is_none(),
            "absent kwargs are not serialized"
        );
        body["scored_samples"][0]["chat_template_kwargs"] =
            json!({"language": "nl", "transcription": true});
        let response: scoring_service::ScoreResponse = serde_json::from_value(body)?;
        let derived = derive_asr("asr_fleurs_nl_nl", response).await?;
        let rows = derived.eval_sample_results.context("asr rows")?;
        let parquet = pipette_core::eval_sample_result::rows_to_parquet_bytes(
            pipette_core::parquet_utils::WriterOpts::default(),
            &rows,
        )?;
        let read = pipette_core::eval_sample_result::rows_from_parquet_bytes(&parquet)?;
        let kwargs: Value = serde_json::from_str(
            read[0]
                .chat_template_kwargs
                .as_deref()
                .context("s1 kwargs")?,
        )?;
        assert_eq!(kwargs, json!({"language": "nl", "transcription": true}));
        assert_eq!(read[1].chat_template_kwargs, None);
        Ok(())
    }

    /// Every FLEURS locale is a word task, named `wer`; a locale the evals
    /// server dropped is a task this build doesn't know, never `cer`.
    #[rstest]
    #[case::english("asr_fleurs_en_us", "wer")]
    #[case::german("asr_fleurs_de_de", "wer")]
    #[case::dutch("asr_fleurs_nl_nl", "wer")]
    #[case::dropped_mandarin("asr_fleurs_cmn_hans_cn", "error_rate")]
    #[case::dropped_japanese("asr_fleurs_ja_jp", "error_rate")]
    #[tokio::test]
    async fn asr_eval_names_wer_for_every_fleurs_locale(
        #[case] task: &str,
        #[case] metric: &str,
    ) -> anyhow::Result<()> {
        let derived = derive_asr(task, asr_response(asr_context("word", 0.08))?).await?;
        assert_eq!(derived.metrics[0].metric, metric);
        Ok(())
    }

    /// URGENT24, the Open ASR Leaderboard's ninth English set, is a known
    /// word task.
    #[tokio::test]
    async fn asr_eval_names_wer_for_urgent2024() -> anyhow::Result<()> {
        let derived = derive_asr(
            "asr_esb_urgent2024",
            asr_response(asr_context("word", 0.12))?,
        )
        .await?;
        assert_eq!(derived.metrics[0].metric, "wer");
        assert!((derived.metrics[0].value - 0.12).abs() < 1e-6);
        Ok(())
    }

    /// A task this build doesn't know is scored as `error_rate`, the metric its
    /// failures are recorded under too.
    #[tokio::test]
    async fn an_unknown_asr_task_scores_as_error_rate() -> anyhow::Result<()> {
        let derived =
            derive_asr("asr_fleurs_th_th", asr_response(asr_context("word", 0.3))?).await?;
        assert_eq!(derived.metrics[0].metric, "error_rate");
        Ok(())
    }

    #[rstest]
    // The character unit is retired: refused for a known task and for one
    // this build doesn't know alike.
    #[case::fleurs_by_character(
        "asr_fleurs_de_de",
        asr_context("character", 0.1),
        "no known error_unit"
    )]
    #[case::urgent2024_by_character(
        "asr_esb_urgent2024",
        asr_context("character", 0.1),
        "no known error_unit"
    )]
    #[case::unknown_task_by_character(
        "asr_fleurs_ja_jp",
        asr_context("character", 0.1),
        "no known error_unit"
    )]
    #[case::no_unit("asr_fleurs_nl_nl", BTreeMap::from([("error_rate".to_string(), json!(0.1))]), "error_unit")]
    #[case::negative_rate("asr_fleurs_nl_nl", asr_context("word", -0.1), "error_rate")]
    #[case::unknown_unit("asr_fleurs_nl_nl", asr_context("phoneme", 0.1), "error_unit")]
    #[case::rate_not_a_number("asr_fleurs_nl_nl", BTreeMap::from([("error_unit".to_string(), json!("word")), ("error_rate".to_string(), json!("0.1"))]), "no error rate")]
    #[case::no_rate("asr_fleurs_nl_nl", BTreeMap::from([("error_unit".to_string(), json!("word")), ("error_rate".to_string(), Value::Null)]), "no error rate")]
    #[case::samples_missing("asr_fleurs_nl_nl", BTreeMap::from([("error_unit".to_string(), json!("word")), ("error_rate".to_string(), json!(0.1)), ("samples_missing".to_string(), json!(1))]), "1 of asr_fleurs_nl_nl's samples unanswered")]
    // A present count that is not one is malformed, not "none missing".
    #[case::missing_as_float("asr_fleurs_nl_nl", BTreeMap::from([("error_unit".to_string(), json!("word")), ("error_rate".to_string(), json!(0.1)), ("samples_missing".to_string(), json!(1.0))]), "not a count")]
    #[case::missing_as_string("asr_fleurs_nl_nl", BTreeMap::from([("error_unit".to_string(), json!("word")), ("error_rate".to_string(), json!(0.1)), ("samples_missing".to_string(), json!("1"))]), "not a count")]
    #[case::missing_negative("asr_fleurs_nl_nl", BTreeMap::from([("error_unit".to_string(), json!("word")), ("error_rate".to_string(), json!(0.1)), ("samples_missing".to_string(), json!(-1))]), "not a count")]
    #[case::rate_past_f32(
        "asr_fleurs_nl_nl",
        asr_context("word", 1e39),
        "past what the column holds"
    )]
    #[tokio::test]
    async fn a_bad_asr_context_is_refused(
        #[case] task: &str,
        #[case] context: BTreeMap<String, Value>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let Err(err) = derive_asr(task, asr_response(context)?).await else {
            anyhow::bail!("expected a refusal");
        };
        assert!(err.to_string().contains(expected), "{err}");
        Ok(())
    }

    /// A scorer reply without `complete` (the scorer no longer sends it) is
    /// read for its rate; an unknown task's metadata names the unit the
    /// scorer counted, and the samples the leaderboard's filter excluded ride
    /// along.
    #[tokio::test]
    async fn a_scored_run_keeps_its_rate() -> anyhow::Result<()> {
        let derived =
            derive_asr("asr_fleurs_th_th", asr_response(asr_context("word", 0.3))?).await?;
        assert!((derived.metrics[0].value - 0.3).abs() < 1e-6);
        let metadata: Value =
            serde_json::from_str(derived.eval_metadata.as_deref().context("metadata")?)?;
        assert_eq!(metadata["error_unit"], "word");
        assert_eq!(metadata["samples_excluded"], 0);
        assert_eq!(metadata.get("complete"), None);
        Ok(())
    }

    /// An asr_eval is scored only with one answer to each clip, as a vlm_eval
    /// is, read on the rows as stored: a short run, a failed clip (canonical,
    /// legacy, or a row whose two fields disagree), a repeated id, or a failed
    /// duplicate of an answered id is refused, never scored as an empty
    /// transcript. A transcript cut off at the cap, or empty, is an answer.
    #[rstest]
    #[case::complete(&[("s1", "eos"), ("s2", "eos")], None)]
    #[case::cut_off_and_empty(&[("s1", "truncated"), ("s2", "eos_empty")], None)]
    #[case::short(&[("s1", "eos")], Some("1 rows, 1 distinct ids, 0 failed"))]
    #[case::a_failed_clip(&[("s1", "eos"), ("s2", "failure")], Some("1 failed"))]
    #[case::a_legacy_failed_clip(&[("s1", "eos"), ("s2", "legacy_failed")], Some("1 failed"))]
    #[case::a_contradicting_clip(&[("s1", "eos"), ("s2", "failed_but_eos")], Some("1 failed"))]
    #[case::a_repeated_id(&[("s1", "eos"), ("s1", "eos")], Some("1 distinct ids"))]
    #[case::a_failed_duplicate(&[("s1", "eos"), ("s1", "failure"), ("s2", "eos")], Some("3 rows"))]
    #[case::extra_rows(&[("s1", "eos"), ("s2", "eos"), ("s3", "eos")], Some("3 rows"))]
    #[tokio::test]
    async fn an_asr_eval_is_scored_only_with_an_answer_to_every_sample(
        #[case] rows: &[(&str, &str)],
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        let completions: Vec<Value> = rows
            .iter()
            .map(|&(id, kind)| match kind {
                "failure" => json!({"id": id, "completion": "", "stop_reason": "failure"}),
                "legacy_failed" => json!({"id": id, "completion": "", "failed": true}),
                "failed_but_eos" => {
                    json!({"id": id, "completion": "", "failed": true, "stop_reason": "eos"})
                }
                "eos_empty" => json!({"id": id, "completion": "", "stop_reason": "eos"}),
                stop => json!({"id": id, "completion": "a x c", "stop_reason": stop}),
            })
            .collect();
        let submission = test_submission(json!({
            "benchmark_id": "drift-guard",
            "benchmark_type": "asr_eval",
            "observation_dataset_digest": "1".repeat(64),
            "completions": completions,
        }))?;
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::AsrEval(asr_eval_def("asr_fleurs_nl_nl")?),
        };
        // The finalize path, through the same scoring the slow worker runs.
        let derived = derive_metrics(
            &benchmark,
            &submission,
            &Config::default(),
            &test_client()?,
            Some(asr_response(asr_context("word", 0.25))?),
        )
        .await;
        match (derived, refused) {
            (Ok(_), None) => {}
            (Err(e), Some(names)) => {
                let e = format!("{e:#}");
                assert!(e.contains(names), "{e}");
                assert!(e.contains("asr_eval job test is not scored"), "{e}");
                assert!(e.contains("an asr_eval is a result only"), "{e}");
            }
            (got, want) => anyhow::bail!("got {:?}, want refusal {want:?}", got.err()),
        }
        Ok(())
    }

    /// A run short of the entry's samples is refused at finalize too, before
    /// any reply is read.
    #[tokio::test]
    async fn a_run_short_of_the_pinned_count_is_refused() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::AsrEval(AsrEvalBenchmark {
                parameter_sample_count: PositiveCount::try_new(3)?,
                ..asr_eval_def("asr_esb_librispeech_test_clean")?
            }),
        };
        let submission = test_submission(json!({
            "benchmark_id": "drift-guard",
            "benchmark_type": "asr_eval",
            "observation_dataset_digest": "1".repeat(64),
            "completions": [{"id": "s1", "completion": "a x c"}, {"id": "s2", "completion": "d"}],
        }))?;
        let Err(err) = derive_metrics(
            &benchmark,
            &submission,
            &Config::default(),
            &test_client()?,
            Some(asr_response(asr_context("word", 0.25))?),
        )
        .await
        else {
            anyhow::bail!("expected a refusal");
        };
        assert!(
            format!("{err:#}").contains("each of its 3 samples: 2 rows, 2 distinct ids, 0 failed"),
            "{err:#}"
        );
        Ok(())
    }

    /// A scored run's warehouse row holds its rate, is not failed, and keeps
    /// the decoding that ran; a partial submission writes no row at all.
    #[tokio::test]
    async fn an_asr_run_writes_its_rate_and_a_partial_one_nothing() -> anyhow::Result<()> {
        let benchmark = Benchmark {
            org_id: None,
            def: BenchmarkDefinition::AsrEval(asr_eval_def("asr_esb_librispeech_test_clean")?),
        };
        let catalog = HashMap::from([(benchmark.benchmark_id().clone(), benchmark)]);
        let model_table = crate::model_params::ModelParamTable::default();
        let models = PassTable::new(&model_table);
        let submission = |completions: Value| {
            test_submission(json!({
                "benchmark_id": "drift-guard",
                "benchmark_type": "asr_eval",
                "observation_dataset_digest": "1".repeat(64),
                "completions": completions,
            }))
        };
        let partial = score_success(
            &Config::default(),
            &catalog,
            &models,
            &test_client()?,
            submission(json!([{"id": "s1", "completion": "a x c"}]))?,
            Some(asr_response(asr_context("word", 0.25))?),
            &VaultNames::default(),
        )
        .await;
        assert!(
            partial
                .err()
                .is_some_and(|e| format!("{e:#}").contains("1 rows, 1 distinct ids, 0 failed")),
            "a partial run is not scored"
        );
        let submission = submission(
            json!([{"id": "s1", "completion": "a x c"}, {"id": "s2", "completion": "d"}]),
        )?;
        let scored = score_success(
            &Config::default(),
            &catalog,
            &models,
            &test_client()?,
            submission,
            Some(asr_response(asr_context("word", 0.25))?),
            &VaultNames::default(),
        )
        .await?;
        let [row] = scored.metric_rows.as_slice() else {
            anyhow::bail!("not one row: {:?}", scored.metric_rows.len());
        };
        assert_eq!((row.metric.as_str(), row.value), ("wer", Some(0.25)));
        assert_eq!(
            (row.failed, row.failed_reason.as_deref()),
            (Some(false), None)
        );
        // qwen25_omni's card: greedy with top_k 1, the rest off.
        assert_eq!(
            (
                row.parameter_temperature,
                row.parameter_top_k,
                row.parameter_repeat_penalty,
                row.parameter_num_beams
            ),
            (Some(0.0), Some(1), Some(1.0), None)
        );
        Ok(())
    }

    /// The decoding columns record what ran: a beam search's penalties at the
    /// card's (repetition `1.2`, length `1.0`) where unpinned and no sampling
    /// column; Gemma 4's sampled card; MiniCPM-o's
    /// greedy repeat penalty; a profile without a card, only what it pins.
    #[test]
    fn asr_decoding_columns_record_what_ran() -> anyhow::Result<()> {
        let beams = AsrEvalBenchmark {
            parameter_prompt_profile: "minicpm_o".into(),
            parameter_num_beams: Some(pipette_plan_types::NumBeams::try_new(3)?),
            ..asr_eval_def("asr_esb_librispeech_test_clean")?
        };
        assert_eq!(
            asr_decoding_columns(&beams),
            AsrDecodingColumns {
                num_beams: Some(3),
                beam_repetition_penalty: Some(1.2),
                beam_length_penalty: Some(1.0),
                ..AsrDecodingColumns::default()
            }
        );
        let gemma = AsrEvalBenchmark {
            parameter_prompt_profile: "gemma4".into(),
            ..asr_eval_def("asr_esb_librispeech_test_clean")?
        };
        let columns = asr_decoding_columns(&gemma);
        assert_eq!(
            (columns.temperature, columns.top_p, columns.top_k),
            (Some(1.0), Some(0.95), Some(64))
        );
        let greedy = AsrEvalBenchmark {
            parameter_prompt_profile: "minicpm_o".into(),
            ..asr_eval_def("asr_esb_librispeech_test_clean")?
        };
        assert_eq!(asr_decoding_columns(&greedy).repeat_penalty, Some(1.02));
        assert_eq!(asr_decoding_columns(&greedy).top_k, Some(1));
        let unknown = AsrEvalBenchmark {
            parameter_prompt_profile: "phi4".into(),
            parameter_top_p: Some(pipette_plan_types::TopP::try_new(0.9)?),
            ..asr_eval_def("asr_esb_librispeech_test_clean")?
        };
        assert_eq!(
            asr_decoding_columns(&unknown),
            AsrDecodingColumns {
                top_p: Some(0.9),
                ..AsrDecodingColumns::default()
            }
        );
        Ok(())
    }

    /// A failure row's metric follows the task's unit; a task this build
    /// doesn't know still intends its row, as `error_rate`.
    #[rstest]
    #[case::open_asr_english("asr_esb_ami_cleaned", &[("wer", "ratio")])]
    #[case::urgent2024("asr_esb_urgent2024", &[("wer", "ratio")])]
    #[case::urgent2024_clean_is_unknown("asr_esb_urgent2024_clean", &[("error_rate", "ratio")])]
    #[case::fleurs_word("asr_fleurs_de_de", &[("wer", "ratio")])]
    #[case::dropped_korean_is_unknown("asr_fleurs_ko_kr", &[("error_rate", "ratio")])]
    #[case::unknown("asr_fleurs_th_th", &[("error_rate", "ratio")])]
    fn asr_eval_intends_its_units_metric(
        #[case] task: &str,
        #[case] expected: &[(&str, &str)],
    ) -> anyhow::Result<()> {
        let def = BenchmarkDefinition::AsrEval(asr_eval_def(task)?);
        assert_eq!(intended_metrics_of(&def, None), expected);
        assert_eq!(
            intended_metrics_of(&def, None)
                .first()
                .map(|(metric, _)| *metric),
            Some(def.as_asr_eval()?.parameter_asr_task.metric())
        );
        Ok(())
    }

    /// A failure on an unknown asr_eval task is recorded with its row, through
    /// the same path every failure takes.
    #[test]
    fn an_unknown_asr_tasks_failure_records_a_row() -> anyhow::Result<()> {
        let rows = failure_rows(
            Benchmark {
                org_id: None,
                def: BenchmarkDefinition::AsrEval(asr_eval_def("asr_fleurs_th_th")?),
            },
            json!({
                "benchmark_type": "asr_eval",
                "retriable": false,
                "failure_reason": "llama-server ran out of memory",
            }),
            None,
        )?;
        assert_eq!(
            rows.iter()
                .map(|r| (r.metric.as_str(), r.unit.as_str(), r.value))
                .collect::<Vec<_>>(),
            [("error_rate", "ratio", None)]
        );
        assert!(rows[0].failed_reason.is_some());
        Ok(())
    }

    #[test]
    fn build_eval_metadata_returns_none_when_no_failures() {
        assert_eq!(build_eval_metadata(0), None);
    }

    #[test]
    fn build_eval_metadata_emits_samples_failed_when_positive() -> anyhow::Result<()> {
        let s = build_eval_metadata(3).context("expected metadata")?;
        assert_eq!(s, r#"{"samples_failed":3}"#);
        Ok(())
    }

    // ---- build_eval_sample_results -------------------------------------

    fn scored(id: &str, is_correct: bool) -> anyhow::Result<scoring_service::ScoredSample> {
        Ok(scoring_service::ScoredSample {
            id: pipette_plan_types::NonEmptyTrimmedString::try_new(id)?,
            messages: vec![pipette_plan_types::ChatMessage {
                role: pipette_plan_types::ChatRole::User,
                content: pipette_plan_types::MessageContent::Text(format!("prompt for {id}")),
            }],
            completion: if is_correct {
                "B".to_string()
            } else {
                String::new()
            },
            is_correct,
            measures: None,
            chat_template_kwargs: None,
        })
    }

    /// When `failed`, sets both the retiring flag and `stop_reason = failure`,
    /// as a current client does.
    fn completion(
        id: &str,
        failed: bool,
        reason: Option<&str>,
    ) -> anyhow::Result<SampleCompletion> {
        Ok(SampleCompletion {
            id: pipette_plan_types::NonEmptyTrimmedString::try_new(id)?,
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

    fn completion_with_stop(
        id: &str,
        stop_reason: &str,
        completion_tokens: i64,
    ) -> anyhow::Result<SampleCompletion> {
        Ok(SampleCompletion {
            id: pipette_plan_types::NonEmptyTrimmedString::try_new(id)?,
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
        // A bare `failed` flag with no `stop_reason` does not count.
        let mut flag_only = completion("b", false, None)?;
        flag_only.failed = true;
        assert!(!is_failed_sample(&flag_only));
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

    /// `enable_thinking` is read off `requested_model_flags`, never the client's
    /// report.
    #[rstest]
    #[case::thinking_on(
        Some(r#"{"benchmark_type":"eval","enable_thinking":true,"model_type":"gguf_text"}"#),
        Some(true)
    )]
    #[case::thinking_off(
        Some(r#"{"benchmark_type":"eval","enable_thinking":false,"model_type":"torch"}"#),
        Some(false)
    )]
    #[case::vlm_eval_thinking_on(
        Some(r#"{"benchmark_type":"vlm_eval","enable_thinking":true,"model_type":"gguf_vision"}"#),
        Some(true)
    )]
    #[case::vlm_eval_instruct(
        Some(r#"{"benchmark_type":"vlm_eval","model_type":"gguf_vision"}"#),
        None
    )]
    #[case::knob_unset(Some(r#"{"benchmark_type":"eval","model_type":"gguf_text"}"#), None)]
    #[case::no_request_recorded(None, None)]
    #[case::not_model_flags(Some("--n-gpu-layers 999"), None)]
    fn requested_enable_thinking_reads_the_job_model_flags(
        #[case] requested: Option<&str>,
        #[case] expected: Option<bool>,
    ) -> anyhow::Result<()> {
        // The client's own report says thinking on in every case.
        let submission = test_submission(json!({
            "benchmark_type": "eval",
            "model_flags": "enable_thinking=true",
            "requested_model_flags": requested,
        }))?;
        assert_eq!(requested_enable_thinking(&submission), expected);
        Ok(())
    }

    /// A vlm_eval run the server did not queue carries no stamp, and is
    /// scored in its recipe's mode; a stamp, which ingestion held to the
    /// recipe, is read first.
    #[rstest]
    #[case::sync_run_thinking_recipe(None, Some(true), Some(true))]
    #[case::sync_run_instruct_recipe(None, None, None)]
    #[case::stamped(
        Some(r#"{"benchmark_type":"vlm_eval","enable_thinking":true,"model_type":"gguf_vision"}"#),
        None,
        Some(true)
    )]
    // A stamp without a mode, or one that does not read, is not overruled.
    #[case::stamped_without_a_mode(
        Some(r#"{"benchmark_type":"vlm_eval","model_type":"gguf_vision"}"#),
        Some(true),
        None
    )]
    #[case::stamped_unreadably(Some("--n-gpu-layers 999"), Some(true), None)]
    fn a_vlm_eval_is_scored_in_its_recipes_thinking_mode(
        #[case] requested: Option<&str>,
        #[case] recipe_mode: Option<bool>,
        #[case] expected: Option<bool>,
    ) -> anyhow::Result<()> {
        let submission = test_submission(json!({
            "benchmark_type": "vlm_eval",
            "requested_model_flags": requested,
        }))?;
        let mut recipe = json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"],
        });
        if let Some(on) = recipe_mode {
            recipe["enable_thinking"] = json!(on);
        }
        let recipe: VlmEvalRecipe = serde_json::from_value(recipe)?;
        assert_eq!(vlm_eval_thinking(&submission, Some(&recipe)), expected);
        Ok(())
    }

    /// A vlm_eval is scored only as the corpus it pins: a run that reports
    /// none, or another, is refused; one that ran it sends the evals server's
    /// digest of those samples to `/score`.
    /// An asr_eval is scored only as the corpus it pins, as a vlm_eval is, and
    /// `/score` is then sent the pinned samples digest; an entry without a
    /// pin is not scored at all.
    #[rstest]
    #[case::none_reported(
        None,
        true,
        Err("job test is not scored: observation_dataset_digest (none")
    )]
    #[case::another_corpus(Some("3"), true, Err("observation_dataset_digest 333"))]
    #[case::unpinned_entry(Some("1"), false, Err("pins no corpus"))]
    #[case::the_pinned_corpus(Some("1"), true, Ok(()))]
    fn an_asr_eval_is_scored_only_as_the_corpus_it_pins(
        #[case] reported: Option<&str>,
        #[case] pinned: bool,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let mut asr = asr_eval_def("asr_fleurs_nl_nl")?;
        if !pinned {
            asr.dataset = None;
        }
        let mut wire = json!({
            "benchmark_id": "asr_eval_fleurs_nl_nl_voxtral",
            "benchmark_type": "asr_eval",
            "completions": [{"id": "s1", "completion": "hallo"}],
        });
        if let Some(digit) = reported {
            wire["observation_dataset_digest"] = json!(digit.repeat(64));
        }
        let submission = test_submission(wire)?;
        match (asr_ran_the_pinned_corpus(&submission, &asr), expected) {
            (Ok(digest), Ok(())) => assert_eq!(digest, format!("sha256:{}", "2".repeat(64))),
            (Err(e), Err(names)) => assert!(format!("{e:#}").contains(names), "{e:#}"),
            (got, want) => anyhow::bail!("got {got:?}, want {want:?}"),
        }
        Ok(())
    }

    /// `/score` hears the recipe's thinking mode, off; an entry with no recipe
    /// (none loads) would send none.
    #[test]
    fn an_asr_eval_scores_with_its_recipes_thinking() -> anyhow::Result<()> {
        let asr = asr_eval_def("asr_fleurs_nl_nl")?;
        assert_eq!(asr_eval_thinking(&asr), Some(false));
        let bare = AsrEvalBenchmark {
            recipe: None,
            ..asr
        };
        assert_eq!(asr_eval_thinking(&bare), None);
        Ok(())
    }

    #[rstest]
    #[case::none_reported(None, Err("(none reported)"))]
    #[case::another_corpus(Some("3"), Err("observation_dataset_digest 333"))]
    #[case::the_pinned_corpus(Some("1"), Ok(()))]
    fn a_vlm_eval_is_scored_only_as_the_corpus_it_pins(
        #[case] reported: Option<&str>,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let entry = Benchmark::from_toml(
            "vlm_eval_mmstar",
            &format!(
                "benchmark_type = \"vlm_eval\"\nparameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 1\n\
                 parameter_max_tokens = 64\n{}",
                pipette_core::dataset::vlm_eval_catalog_tables("mmstar", "2026.10.1", None)
            ),
        )?;
        let dataset = entry
            .dataset()
            .ok_or_else(|| anyhow::anyhow!("the entry pins a corpus"))?;
        let mut wire = json!({
            "benchmark_id": "vlm_eval_mmstar",
            "benchmark_type": "vlm_eval",
            "completions": [{"id": "s1", "completion": "B"}],
        });
        if let Some(digit) = reported {
            wire["observation_dataset_digest"] = json!(digit.repeat(64));
        }
        let submission = test_submission(wire)?;
        match (ran_the_pinned_corpus(&submission, dataset), expected) {
            (Ok(digest), Ok(())) => assert_eq!(digest, format!("sha256:{}", "2".repeat(64))),
            (Err(e), Err(names)) => assert!(format!("{e:#}").contains(names), "{e:#}"),
            (got, want) => anyhow::bail!("got {got:?}, want {want:?}"),
        }
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

        let err = build_eval_sample_results(&completions, scored)
            .err()
            .context("build_eval_sample_results must reject this")?;
        let msg = format!("{err}");
        assert!(msg.contains("dropped"));
        assert!(msg.contains("bad"));
        Ok(())
    }

    #[test]
    fn build_eval_sample_results_bails_if_scorer_drops_a_non_failed_row() -> anyhow::Result<()> {
        // Any dropped row, not just failed ones, else accuracy's denominator
        // skews silently.
        let completions = vec![completion("a", false, None)?, completion("b", false, None)?];
        let scored = vec![scored("a", true)?];

        let err = build_eval_sample_results(&completions, scored)
            .err()
            .context("build_eval_sample_results must reject this")?;
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

        let err = build_eval_sample_results(&completions, scored)
            .err()
            .context("build_eval_sample_results must reject this")?;
        let msg = format!("{err}");
        assert!(msg.contains("unknown"));
        assert!(msg.contains("ghost"));
        Ok(())
    }

    /// A vlm_eval row keeps its prompt's text, system message included, and
    /// drops only the base64 images.
    #[tokio::test]
    async fn a_vlm_eval_row_keeps_its_prompt_text_without_its_images() -> anyhow::Result<()> {
        let benchmark = Benchmark::from_toml(
            "vlm_eval_mmstar",
            &format!(
                "benchmark_type = \"vlm_eval\"\n\
                 parameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"2026.10.1\"\n\
                 parameter_sample_count = 1\n\
                 parameter_max_tokens = 8192\n{}",
                pipette_core::dataset::vlm_eval_catalog_tables("mmstar", "2026.10.1", None)
            ),
        )?;
        let submission = test_submission(json!({
            "benchmark_id": "vlm_eval_mmstar",
            "benchmark_type": "vlm_eval",
            "observation_dataset_digest": "1".repeat(64),
            "completions": [{"id": "s1", "completion": "B"}],
        }))?;
        let score = serde_json::from_value(json!({
            "runtime_version": "scorer-1",
            "context": {"accuracy": 1.0},
            "scored_samples": [{
                "id": "s1",
                "messages": [
                    {"role": "system", "content": "Answer with a letter."},
                    {"role": "user", "content": [
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}},
                        {"type": "text", "text": "Which shape is red?"},
                    ]},
                ],
                "completion": "B",
                "is_correct": true,
            }],
        }))?;

        let derived = derive_metrics(
            &benchmark,
            &submission,
            &Config::default(),
            &test_client()?,
            Some(score),
        )
        .await?;

        let rows = derived
            .eval_sample_results
            .context("a vlm_eval stores its samples")?;
        assert_eq!(
            rows.iter().map(|r| r.messages.as_str()).collect::<Vec<_>>(),
            [
                r#"[{"role":"system","content":"Answer with a letter."},{"role":"user","content":[{"type":"text","text":"Which shape is red?"}]}]"#
            ]
        );
        Ok(())
    }

    fn vlm_eval_mmstar(samples: u32) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            "vlm_eval_mmstar",
            &format!(
                "benchmark_type = \"vlm_eval\"\nparameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = {samples}\n\
                 parameter_max_tokens = 64\n{}",
                pipette_core::dataset::vlm_eval_catalog_tables("mmstar", "2026.10.1", None)
            ),
        )
    }

    /// A vlm_eval is scored only with one answer to each sample, read on the
    /// rows as stored: a short run, a failed row (canonical or legacy), a
    /// repeated id, or a failed duplicate of an answered id is refused, never
    /// scored as wrong. A reply cut off at the cap is an answer.
    #[rstest]
    #[case::complete(&[("s1", "eos"), ("s2", "eos")], None)]
    #[case::cut_off_and_empty(&[("s1", "truncated"), ("s2", "eos_empty")], None)]
    #[case::short(&[("s1", "eos")], Some("1 rows, 1 distinct ids, 0 failed"))]
    #[case::a_failed_row(&[("s1", "eos"), ("s2", "failure")], Some("1 failed"))]
    #[case::a_legacy_failed_row(&[("s1", "eos"), ("s2", "legacy_failed")], Some("1 failed"))]
    #[case::a_repeated_id(&[("s1", "eos"), ("s1", "eos")], Some("1 distinct ids"))]
    #[case::a_failed_duplicate(&[("s1", "eos"), ("s1", "failure"), ("s2", "eos")], Some("3 rows"))]
    #[case::extra_rows(&[("s1", "eos"), ("s2", "eos"), ("s3", "eos")], Some("3 rows"))]
    #[tokio::test]
    async fn a_vlm_eval_is_scored_only_with_an_answer_to_every_sample(
        #[case] rows: &[(&str, &str)],
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        let completions: Vec<Value> = rows
            .iter()
            .map(|&(id, kind)| match kind {
                "failure" => json!({"id": id, "completion": "", "stop_reason": "failure"}),
                "legacy_failed" => json!({"id": id, "completion": "", "failed": true}),
                "eos_empty" => json!({"id": id, "completion": "", "stop_reason": "eos"}),
                stop => json!({"id": id, "completion": "B", "stop_reason": stop}),
            })
            .collect();
        let submission = test_submission(json!({
            "benchmark_id": "vlm_eval_mmstar",
            "benchmark_type": "vlm_eval",
            "observation_dataset_digest": "1".repeat(64),
            "completions": completions,
        }))?;
        let ids: Vec<&str> = rows.iter().map(|&(id, _)| id).collect();
        let score = serde_json::from_value(json!({
            "runtime_version": "scorer-1",
            "context": {"accuracy": 0.5, "samples_missing": 0},
            "scored_samples": ids.iter().collect::<BTreeSet<_>>().iter()
                .map(|id| json!({"id": id, "messages": [], "completion": "B", "is_correct": true}))
                .collect::<Vec<_>>(),
        }))?;
        // The finalize path, through the same scoring the slow worker runs.
        let derived = derive_metrics(
            &vlm_eval_mmstar(2)?,
            &submission,
            &Config::default(),
            &test_client()?,
            Some(score),
        )
        .await;
        match (derived, refused) {
            (Ok(_), None) => {}
            (Err(e), Some(names)) => assert!(format!("{e:#}").contains(names), "{e:#}"),
            (got, want) => anyhow::bail!("got {:?}, want refusal {want:?}", got.err()),
        }
        Ok(())
    }

    /// The headline is the result: a scorer reply without a ratio for it, or
    /// one that finds samples unanswered, is not finalized.
    #[rstest]
    #[case::no_headline(json!({}), "no headline ratio")]
    #[case::not_a_ratio(json!({"accuracy": 1.5}), "no headline ratio")]
    #[case::samples_missing(json!({"accuracy": 0.5, "samples_missing": 1}), "1 of mmstar's samples")]
    #[tokio::test]
    async fn a_vlm_eval_score_without_its_headline_is_not_finalized(
        #[case] context: Value,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let submission = test_submission(json!({
            "benchmark_id": "vlm_eval_mmstar",
            "benchmark_type": "vlm_eval",
            "observation_dataset_digest": "1".repeat(64),
            "completions": [{"id": "s1", "completion": "B"}],
        }))?;
        let score = serde_json::from_value(json!({
            "runtime_version": "scorer-1",
            "context": context,
            "scored_samples": [{"id": "s1", "messages": [], "completion": "B", "is_correct": true}],
        }))?;
        let Err(e) = derive_metrics(
            &vlm_eval_mmstar(1)?,
            &submission,
            &Config::default(),
            &test_client()?,
            Some(score),
        )
        .await
        else {
            anyhow::bail!("a score without its headline must not be finalized");
        };
        assert!(e.to_string().contains(names), "{e}");
        Ok(())
    }

    // ── commit_chunk: per-job isolation ───────────────────────────────────

    use std::sync::Arc;

    use async_trait::async_trait;

    use pipette_core::config::{Config as CommitConfig, StorageConfig};
    use pipette_core::warehouse::MetricRow;
    use pipette_plan_types::ClientId;
    use pipette_store::{EvalSampleResultStore, JobState, Stores};
    use pipette_store_fs::build_local_fs_stores;

    fn commit_test_stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = CommitConfig {
            evals_server_url: "http://unused".to_string(),
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..CommitConfig::default()
        };
        build_local_fs_stores(&config)
    }

    /// Eval-results store that always fails `write`.
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
            device_name: Some("d".to_string()),
            device_os_name: Some("linux".to_string()),
            device_chip_model: Some("chip".to_string()),
            device_ram_bytes: Some(16_000_000_000),
            model_name: Some("m".to_string()),
            model_quant: Some("q4_0".to_string()),
            value: Some(34.7),
            unit: "ms".to_string(),
            ..Default::default()
        })
    }

    /// A scored job with one row. A test fails `mark_processed` for a job by
    /// not writing its incoming body.
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

    #[tokio::test]
    async fn commit_chunk_isolates_a_failing_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;

        // "poison" has no incoming body, so its mark_processed errors.
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
        assert!(
            stores
                .submissions
                .get_submission(&JobId::new_unchecked("poison"))
                .await?
                .is_none()
        );
        Ok(())
    }

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

    #[tokio::test]
    async fn commit_chunk_errors_when_all_jobs_fail() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;
        let outcomes = vec![commit_scored_job("a")?, commit_scored_job("b")?];
        let err = commit_chunk(&stores, &outcomes)
            .await
            .err()
            .context("all-failed commit should surface an error")?;
        assert!(err.to_string().contains("commit failed for all"));
        Ok(())
    }

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

        // Only `poison` carries eval results, so only its write fails.
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
        let poison_rec = stores
            .submissions
            .get_submission(&JobId::new_unchecked("poison"))
            .await?
            .context("poison job should still be findable")?;
        assert!(matches!(poison_rec.state, JobState::Incoming));
        Ok(())
    }

    /// A recorded failure with no rows is deferred alone: the healthy job in
    /// its chunk still commits, and nothing writes an empty partition.
    #[tokio::test]
    async fn commit_chunk_defers_a_rowless_failure_alone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;
        for jid in ["good", "rowless"] {
            stores
                .submissions
                .write_incoming(
                    &JobId::new_unchecked(jid),
                    &json!({"message_type": "failure"}),
                )
                .await?;
        }
        let rowless = ScoreOutcome::RecordedFailure(ScoredJob {
            job_id: JobId::new_unchecked("rowless"),
            benchmark_id: BenchmarkId::try_new("other_bench")?,
            client_id: ClientId::try_new("c1")?,
            day_key: "2026-06-12".to_string(),
            metric_rows: Vec::new(),
            eval_sample_results: None,
        });

        let deferred = commit_chunk(&stores, &[commit_scored_job("good")?, rowless]).await?;

        assert_eq!(deferred, vec![JobId::new_unchecked("rowless")]);
        let good = stores
            .submissions
            .get_submission(&JobId::new_unchecked("good"))
            .await?
            .context("good job should still be findable")?;
        assert!(matches!(good.state, JobState::Processed));
        let rowless = stores
            .submissions
            .get_submission(&JobId::new_unchecked("rowless"))
            .await?
            .context("the rowless job stays")?;
        assert!(matches!(rowless.state, JobState::Incoming));
        Ok(())
    }

    // ── fast worker: eval routing ─────────────────────────────────────────

    use std::collections::HashMap as StdHashMap;
    use std::num::NonZeroUsize;

    use pipette_store::ScoreQueueStage;

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

    /// `evals_server_url` points nowhere; a routed job must not touch it.
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
                org_id: None,
                def: BenchmarkDefinition::Eval(EvalBenchmark {
                    benchmark_id: BenchmarkId::try_new("eval_test")?,
                    parameter_eval_id: "e".into(),
                    parameter_dataset_name: "d".to_string(),
                    parameter_max_tokens: PositiveCount::try_new(16)?,
                    parameter_max_context: None,
                    parameter_mcq_choices: None,
                    parameter_temperature: None,
                    parameter_top_p: None,
                    parameter_top_k: None,
                    parameter_min_p: None,
                    parameter_seed: None,
                    samples: None,
                }),
            },
        );

        let job = JobId::new_unchecked("evaljob");
        stores
            .submissions
            .write_incoming(&job, &incoming_body("evaljob", "eval_test", "eval"))
            .await?;

        let http_client = test_client()?;
        let empty = ModelParamTable::empty();
        let outcome = score_chunk(
            &config,
            &catalog,
            &PassTable::new(&empty),
            &stores,
            &mut RerunParents::new(&*stores.plans),
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

    fn prefill_catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let benchmark = prefill_benchmark()?;
        Ok(HashMap::from([(
            benchmark.benchmark_id().clone(),
            benchmark,
        )]))
    }

    async fn put_rerun_plan(
        stores: &Stores,
        rerun_of: &[(&str, &str)],
    ) -> anyhow::Result<pipette_core::plan::PlanManifest> {
        let manifest = pipette_core::plan::PlanManifest {
            rerun_of: rerun_of
                .iter()
                .map(|(job, parent)| (JobId::new_unchecked(*job), JobId::new_unchecked(*parent)))
                .collect(),
            ..pipette_core::plan::sample_manifest(
                PlanId::from_uuid(uuid::Uuid::now_v7()),
                pipette_plan_types::PlanStatus::Active,
                false,
            )?
        };
        stores.plans.put_plan(&manifest).await?;
        Ok(manifest)
    }

    async fn write_prefill(stores: &Stores, job_id: &JobId, outcome: &Value) -> anyhow::Result<()> {
        let mut body = incoming_body(
            job_id.as_str(),
            "prefill_throughput_256",
            "prefill_throughput",
        );
        body.as_object_mut()
            .context("the incoming body is a JSON object")?
            .extend(outcome.as_object().context("a JSON object")?.clone());
        stores.submissions.write_incoming(job_id, &body).await
    }

    /// Measured and failed rows alike; a job the plan did not re-run names none.
    #[rstest]
    #[case::measured(json!({ "prefill_time_ms": 34.7 }))]
    #[case::failed(json!({
        "message_type": "failure",
        "submitted_at": Utc::now().to_rfc3339(),
        "retriable": false,
        "failure_reason": "OOM",
    }))]
    #[tokio::test]
    async fn score_chunk_names_the_job_a_rerun_runs_again(
        #[case] outcome: Value,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;
        let manifest = put_rerun_plan(&stores, &[("job-2", "job-0")]).await?;
        let jobs = [JobId::new_unchecked("job-1"), JobId::new_unchecked("job-2")];
        for job_id in &jobs {
            stores
                .plans
                .put_job_plan_link(job_id, &manifest.plan_id)
                .await?;
            write_prefill(&stores, job_id, &outcome).await?;
        }

        let empty = ModelParamTable::empty();
        let scored = score_chunk(
            &CommitConfig::default(),
            &prefill_catalog()?,
            &PassTable::new(&empty),
            &stores,
            &mut RerunParents::new(&*stores.plans),
            &test_client()?,
            1,
            &jobs,
        )
        .await;

        assert!(scored.newly_failed.is_empty(), "{:?}", scored.newly_failed);
        // A set of every row's pair: a row naming the wrong parent adds a pair.
        let rows: std::collections::BTreeSet<(&str, Option<&str>)> = scored
            .outcomes
            .iter()
            .flat_map(|o| match o {
                ScoreOutcome::Scored(job) | ScoreOutcome::RecordedFailure(job) => {
                    job.metric_rows.as_slice()
                }
                ScoreOutcome::ArchivedLockedFailure(_) | ScoreOutcome::Routed { .. } => &[],
            })
            .map(|r| {
                (
                    r.job_id.as_deref().unwrap_or_default(),
                    r.rerun_of.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            std::collections::BTreeSet::from([("job-1", None), ("job-2", Some("job-0"))])
        );
        Ok(())
    }

    /// Any linking plan may name the parent, a gone plan names none, and each
    /// manifest is read once.
    #[tokio::test]
    async fn rerun_parents_reads_every_plan_that_links_the_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = commit_test_stores(dir.path())?;
        // A plan that also holds `joined` without re-running it, beside the
        // revision that re-ran it.
        let holder = put_rerun_plan(&stores, &[]).await?;
        let revision = put_rerun_plan(&stores, &[("joined", "job-0"), ("cached", "job-0")]).await?;
        let gone = PlanId::from_uuid(uuid::Uuid::now_v7());
        let links = [
            ("joined", &holder.plan_id),
            ("joined", &revision.plan_id),
            ("cached", &revision.plan_id),
            ("orphan", &gone),
        ];
        for (job, plan_id) in links {
            stores
                .plans
                .put_job_plan_link(&JobId::new_unchecked(job), plan_id)
                .await?;
        }
        let mut parents = RerunParents::new(&*stores.plans);

        let cases = [
            ("joined", Some("job-0")),
            ("orphan", None),
            ("unlinked", None),
        ];
        for (job, want) in cases {
            let got = parents.of(&JobId::new_unchecked(job)).await?;
            assert_eq!(got.as_ref().map(JobId::as_str), want, "{job}");
        }

        // `joined` read the revision, so `cached` is answered from that read.
        stores
            .plans
            .put_plan(&pipette_core::plan::PlanManifest {
                rerun_of: BTreeMap::new(),
                ..revision
            })
            .await?;
        let cached = parents.of(&JobId::new_unchecked("cached")).await?;
        assert_eq!(cached.as_ref().map(JobId::as_str), Some("job-0"));
        Ok(())
    }

    /// The job stays in `incoming/` rather than land rows without the link.
    #[tokio::test]
    async fn score_chunk_defers_a_job_whose_plans_do_not_list() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = Stores {
            plans: Arc::new(pipette_store_testkit::PlanStoreUnimplemented),
            ..commit_test_stores(dir.path())?
        };
        let job = JobId::new_unchecked("job-1");
        write_prefill(&stores, &job, &json!({ "prefill_time_ms": 34.7 })).await?;

        let empty = ModelParamTable::empty();
        let scored = score_chunk(
            &CommitConfig::default(),
            &prefill_catalog()?,
            &PassTable::new(&empty),
            &stores,
            &mut RerunParents::new(&*stores.plans),
            &test_client()?,
            1,
            std::slice::from_ref(&job),
        )
        .await;

        assert_eq!(scored.newly_failed, vec![job.clone()]);
        assert!(scored.outcomes.is_empty());
        assert_eq!(
            stores.submissions.list_incoming(NonZeroUsize::MAX).await?,
            vec![job]
        );
        Ok(())
    }

    // ── vault labels ──────────────────────────────────────────────────────

    use pipette_core::canonical_json::{canonicalize_str, sha256_hex};
    use pipette_core::vault::VaultResource;
    use pipette_store_testkit::a_vault_runtime;

    use crate::vault_label::{ran, reported};

    fn reporting(mut outcome: Value, model: &str, runtime: &str) -> anyhow::Result<Value> {
        outcome
            .as_object_mut()
            .context("the outcome is a JSON object")?
            .extend([
                ("model_descriptor".to_string(), json!(model)),
                ("runtime_descriptor".to_string(), json!(runtime)),
            ]);
        Ok(outcome)
    }

    async fn stores_holding(
        dir: &std::path::Path,
        records: &[&VaultResource],
    ) -> anyhow::Result<Stores> {
        let stores = commit_test_stores(dir)?;
        futures::future::try_join_all(records.iter().map(|r| stores.vault.put_resource(r))).await?;
        Ok(stores)
    }

    async fn score_incoming(stores: &Stores) -> anyhow::Result<ChunkOutcome> {
        let chunk = stores.submissions.list_incoming(NonZeroUsize::MAX).await?;
        anyhow::ensure!(chunk.len() == 1, "expected one incoming job, got {chunk:?}");
        let empty = ModelParamTable::empty();
        Ok(score_chunk(
            &CommitConfig::default(),
            &prefill_catalog()?,
            &PassTable::new(&empty),
            stores,
            &mut RerunParents::new(&*stores.plans),
            &test_client()?,
            1,
            &chunk,
        )
        .await)
    }

    async fn submit_prefill(stores: &Stores, outcome: &Value) -> anyhow::Result<()> {
        write_prefill(stores, &JobId::new_unchecked("job-1"), outcome).await
    }

    async fn incoming_rows(stores: &Stores) -> anyhow::Result<Vec<MetricRow>> {
        let scored = score_incoming(stores).await?;
        anyhow::ensure!(scored.newly_failed.is_empty(), "the job was deferred");
        let rows: Vec<MetricRow> = scored
            .outcomes
            .into_iter()
            .flat_map(|o| match o {
                ScoreOutcome::Scored(job) | ScoreOutcome::RecordedFailure(job) => job.metric_rows,
                ScoreOutcome::ArchivedLockedFailure(_) | ScoreOutcome::Routed { .. } => Vec::new(),
            })
            .collect();
        anyhow::ensure!(!rows.is_empty(), "scoring produced no rows");
        Ok(rows)
    }

    /// Where a job's descriptors come from, and whether with their labels.
    #[derive(Clone, Copy)]
    enum Reported {
        Measured {
            labelled: bool,
        },
        /// Written by the server from a job body filed without a label.
        ServerFailure,
    }

    /// The filled label goes under the id of the unlabelled text.
    #[rstest]
    #[case::measured(Reported::Measured { labelled: false })]
    #[case::server_failure(Reported::ServerFailure)]
    #[case::labelled(Reported::Measured { labelled: true })]
    #[tokio::test]
    async fn score_chunk_labels_the_vault_sources_a_result_leaves_unlabelled(
        #[case] from: Reported,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let model = pipette_store_testkit::a_vault_resource()?;
        let runtime = a_vault_runtime()?;
        let stores = stores_holding(dir.path(), &[&model, &runtime]).await?;
        ran(&stores, "job-1", &[&model, &runtime]).await?;
        match from {
            Reported::Measured { labelled } => {
                let outcome = reporting(
                    json!({ "prefill_time_ms": 34.7 }),
                    &reported(&model, labelled)?,
                    &reported(&runtime, labelled)?,
                )?;
                submit_prefill(&stores, &outcome).await?;
            }
            Reported::ServerFailure => {
                let job_body = json!({
                    "job_id": "job-1",
                    "spec": {
                        "benchmark": "prefill_throughput_256",
                        "model": serde_json::from_str::<Value>(&reported(&model, false)?)?,
                        "runtime": serde_json::from_str::<Value>(&reported(&runtime, false)?)?,
                    },
                });
                crate::submission::record_system_failure(
                    stores.submissions.as_ref(),
                    &prefill_catalog()?,
                    &job_body,
                    "no client claimed the job",
                    FailureSource::Expired,
                    Utc::now(),
                )
                .await?;
            }
        }

        let rows = incoming_rows(&stores).await?;

        let (model_text, runtime_text) = (reported(&model, true)?, reported(&runtime, true)?);
        assert!(model_text.contains(r#""custom_label":"private checkpoint""#));
        assert!(runtime_text.contains(r#""custom_label":"acme llama.cpp build""#));
        let model_id = sha256_hex(&reported(&model, false)?);
        let runtime_id = sha256_hex(&reported(&runtime, false)?);
        rows.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(row.model_descriptor.as_ref(), Some(&model_text), "{id}");
            assert_eq!(row.runtime_descriptor.as_ref(), Some(&runtime_text), "{id}");
            assert_eq!(row.model_vault_descriptor, None, "{id}");
            assert_eq!(
                row.model_descriptor_sha256.as_ref(),
                Some(&model_id),
                "{id}"
            );
            assert_eq!(
                row.runtime_descriptor_sha256.as_ref(),
                Some(&runtime_id),
                "{id}"
            );
        });
        Ok(())
    }

    /// Scoring fills a missing label and corrects none.
    #[tokio::test]
    async fn score_chunk_keeps_a_reported_label() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let model = pipette_store_testkit::a_vault_resource()?;
        let runtime = a_vault_runtime()?;
        let stores = stores_holding(dir.path(), &[&model, &runtime]).await?;
        let mut renamed: Value = serde_json::from_str(&reported(&model, false)?)?;
        renamed["custom_label"] = json!("another name");
        let renamed = renamed.to_string();
        let outcome = reporting(
            json!({ "prefill_time_ms": 34.7 }),
            &renamed,
            &reported(&runtime, true)?,
        )?;
        submit_prefill(&stores, &outcome).await?;

        let renamed = canonicalize_str(&renamed);
        incoming_rows(&stores).await?.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(row.model_descriptor.as_ref(), Some(&renamed), "{id}");
        });
        Ok(())
    }

    /// An imported file takes no label, beside a labelled upload.
    #[tokio::test]
    async fn score_chunk_leaves_an_imported_model_unlabelled() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let model = VaultResource {
            origin: Some(pipette_store_testkit::a_huggingface_origin()?),
            ..pipette_store_testkit::a_vault_resource()?
        };
        let runtime = a_vault_runtime()?;
        let stores = stores_holding(dir.path(), &[&model, &runtime]).await?;
        ran(&stores, "job-1", &[&model, &runtime]).await?;
        let vault_model = reported(&model, false)?;
        let alias = crate::vault_alias::model_alias(
            &*stores.vault,
            Some(&vault_model),
            &KnownOrigins::new(),
        )
        .await?
        .context("an imported file has an alias")?;
        let outcome = reporting(
            json!({ "prefill_time_ms": 34.7 }),
            &vault_model,
            &reported(&runtime, false)?,
        )?;
        submit_prefill(&stores, &outcome).await?;

        let rows = incoming_rows(&stores).await?;

        let alias = canonicalize_str(&alias);
        assert!(!alias.contains("vault") && !alias.contains("custom_label"));
        let labelled_runtime = reported(&runtime, true)?;
        rows.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(row.model_descriptor.as_ref(), Some(&alias), "{id}");
            assert_eq!(
                row.model_descriptor_sha256,
                Some(sha256_hex(&alias)),
                "{id}"
            );
            assert_eq!(
                row.model_vault_descriptor.as_ref(),
                Some(&vault_model),
                "{id}"
            );
            assert_eq!(
                row.runtime_descriptor.as_ref(),
                Some(&labelled_runtime),
                "{id}"
            );
        });
        Ok(())
    }

    #[tokio::test]
    async fn score_chunk_records_a_source_with_no_record_as_reported() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let gone = pipette_store_testkit::a_vault_resource()?;
        let runtime = a_vault_runtime()?;
        let stores = stores_holding(dir.path(), &[&runtime]).await?;
        ran(&stores, "job-1", &[&gone, &runtime]).await?;
        let unlabelled_model = reported(&gone, false)?;
        let outcome = reporting(
            json!({ "prefill_time_ms": 34.7 }),
            &unlabelled_model,
            &reported(&runtime, false)?,
        )?;
        submit_prefill(&stores, &outcome).await?;

        let rows = incoming_rows(&stores).await?;

        let labelled_runtime = reported(&runtime, true)?;
        rows.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(
                row.model_descriptor.as_ref(),
                Some(&unlabelled_model),
                "{id}"
            );
            assert_eq!(
                row.runtime_descriptor.as_ref(),
                Some(&labelled_runtime),
                "{id}"
            );
        });
        Ok(())
    }

    /// What job-1's stored source is when its model descriptor names `theirs`.
    enum JobSource {
        NamesOnlyTheRuntime,
        /// Names both, but the model descriptor drops `principal_id`: the
        /// labeller still reads a malformed source's ids.
        NamesBothToAnUnprincipledModel,
        Missing,
        NotJson,
    }

    /// A device may report any principal's record, so a source the job's body
    /// does not name gives the row neither that record's name nor its origin.
    /// A job with no readable source fails closed rather than stall.
    #[rstest]
    #[case::uploaded(false, JobSource::NamesOnlyTheRuntime)]
    #[case::imported(true, JobSource::NamesOnlyTheRuntime)]
    #[case::unprincipled(true, JobSource::NamesBothToAnUnprincipledModel)]
    #[case::unsourced_upload(false, JobSource::Missing)]
    #[case::unsourced_import(true, JobSource::Missing)]
    #[case::unreadable(true, JobSource::NotJson)]
    #[tokio::test]
    async fn score_chunk_reads_no_record_its_job_does_not_name(
        #[case] imported: bool,
        #[case] job: JobSource,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let origin = imported
            .then(pipette_store_testkit::a_huggingface_origin)
            .transpose()?;
        let theirs = VaultResource {
            origin,
            ..pipette_store_testkit::a_vault_resource()?
        };
        let runtime = a_vault_runtime()?;
        let stores = stores_holding(dir.path(), &[&theirs, &runtime]).await?;
        let mut model_text = reported(&theirs, false)?;
        match job {
            JobSource::NamesOnlyTheRuntime => ran(&stores, "job-1", &[&runtime]).await?,
            JobSource::NamesBothToAnUnprincipledModel => {
                ran(&stores, "job-1", &[&theirs, &runtime]).await?;
                let mut model = serde_json::from_str::<Value>(&model_text)?;
                model
                    .as_object_mut()
                    .context("the descriptor is a JSON object")?
                    .remove("principal_id");
                model_text = canonicalize_str(&model.to_string());
            }
            JobSource::Missing => {}
            JobSource::NotJson => {
                std::fs::write(dir.path().join("job-sources/job-1.json"), b"not json")?;
            }
        }
        let runtime_text = reported(&runtime, false)?;
        let outcome = reporting(
            json!({ "prefill_time_ms": 34.7 }),
            &model_text,
            &runtime_text,
        )?;
        submit_prefill(&stores, &outcome).await?;

        incoming_rows(&stores).await?.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(row.model_descriptor.as_ref(), Some(&model_text), "{id}");
            assert_eq!(row.model_vault_descriptor, None, "{id}");
            assert_eq!(row.runtime_descriptor.as_ref(), Some(&runtime_text), "{id}");
        });
        Ok(())
    }

    /// An unlabelled source defers rather than land rows without the label; a
    /// labelled one reads no record.
    #[rstest]
    #[case::unlabelled(false, true)]
    #[case::labelled(true, false)]
    #[tokio::test]
    async fn score_chunk_defers_a_job_whose_vault_records_do_not_read(
        #[case] labelled: bool,
        #[case] deferred: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = Stores {
            vault: Arc::new(pipette_store_testkit::VaultStoreUnimplemented),
            ..commit_test_stores(dir.path())?
        };
        let runtime = a_vault_runtime()?;
        ran(&stores, "job-1", &[&runtime]).await?;
        // A public model, so the alias reads no record either.
        let outcome = reporting(
            json!({ "prefill_time_ms": 34.7 }),
            &gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_0.gguf"),
            &reported(&runtime, labelled)?,
        )?;
        submit_prefill(&stores, &outcome).await?;

        let scored = score_incoming(&stores).await?;

        assert_eq!(!scored.newly_failed.is_empty(), deferred);
        assert_eq!(scored.outcomes.is_empty(), deferred);
        Ok(())
    }

    #[tokio::test]
    async fn score_chunk_reads_no_record_for_too_many_vault_resources() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = Stores {
            vault: Arc::new(pipette_store_testkit::VaultStoreUnimplemented),
            ..commit_test_stores(dir.path())?
        };
        let sources = (0..=MAX_LABELLED_RESOURCES)
            .map(|_| {
                let source = reported(&a_vault_runtime()?, false)?;
                anyhow::Ok(serde_json::from_str::<Value>(&source)?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let runtime = canonicalize_str(&json!({ "builds": sources }).to_string());
        let outcome = reporting(
            json!({ "prefill_time_ms": 34.7 }),
            &gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_0.gguf"),
            &runtime,
        )?;
        submit_prefill(&stores, &outcome).await?;

        incoming_rows(&stores).await?.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(row.runtime_descriptor.as_ref(), Some(&runtime), "{id}");
        });
        Ok(())
    }

    /// A recalculated eval's copy may predate the label. A copy of a copy that
    /// has no source, as `requeue-eval` writes, is judged by the device run's.
    #[rstest]
    #[case::copy(false, false)]
    #[case::copy_of_a_copy(true, false)]
    #[case::imported_copy_of_a_copy(true, true)]
    #[tokio::test]
    async fn finalize_labels_a_recalculated_eval(
        #[case] copied: bool,
        #[case] imported: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let origin = imported
            .then(pipette_store_testkit::a_huggingface_origin)
            .transpose()?;
        let model = VaultResource {
            origin,
            ..pipette_store_testkit::a_vault_resource()?
        };
        let stores = stores_holding(dir.path(), &[&model]).await?;
        ran(&stores, "job-a", &[&model]).await?;
        let benchmark = eval_benchmark("eval_math_500_2026.06.1", "")?;
        let [device, copy, job] = ["job-a", "job-b", "job-c"].map(JobId::new_unchecked);
        let mut recorded = pipette_store_testkit::success_record(&device, "eval");
        recorded
            .as_object_mut()
            .context("the record is a JSON object")?
            .extend([
                (
                    "benchmark_id".to_string(),
                    json!(benchmark.benchmark_id().as_str()),
                ),
                (
                    "model_descriptor".to_string(),
                    json!(reported(&model, false)?),
                ),
            ]);
        let submissions = stores.submissions.as_ref();
        submissions.write_processed(&device, &recorded).await?;
        let source = if copied {
            crate::recalculation::answer_from(submissions, &device, &copy, Utc::now()).await?;
            copy
        } else {
            device
        };
        crate::recalculation::answer_from(submissions, &source, &job, Utc::now()).await?;
        let copy = submissions.find_job(&job).await?.context("no copy")?.body;
        let score = json!({
            "runtime_version": "scorer-1",
            "scored_samples": [{
                "id": "s1",
                "messages": [{"role": "user", "content": "2 + 2?"}],
                "completion": "@@@@",
                "is_correct": true,
            }],
        });
        submissions
            .enqueue(
                ScoreQueueStage::ToFinalize,
                &job,
                &json!({ "submission": copy, "score": score }),
            )
            .await?;

        let empty = ModelParamTable::empty();
        let finalized = finalize_scored_evals(
            &CommitConfig::default(),
            &HashMap::from([(benchmark.benchmark_id().clone(), benchmark)]),
            &PassTable::new(&empty),
            &test_client()?,
            &stores,
            &mut RerunParents::new(&*stores.plans),
            NonZeroUsize::MIN,
        )
        .await?;

        assert_eq!(finalized, 1);
        let mut rows = Vec::new();
        stores
            .warehouse
            .for_each_metric_row(&mut |row| {
                rows.push(row.clone());
                false
            })
            .await?;
        assert!(!rows.is_empty());
        let unlabelled = reported(&model, false)?;
        let (descriptor, id_text, vault) = if imported {
            let alias = crate::vault_alias::model_alias(
                &*stores.vault,
                Some(&unlabelled),
                &KnownOrigins::new(),
            )
            .await?
            .context("an imported file has an alias")?;
            let alias = canonicalize_str(&alias);
            (alias.clone(), alias, Some(unlabelled))
        } else {
            (reported(&model, true)?, unlabelled, None)
        };
        rows.iter().for_each(|row| {
            let id = &row.result_id;
            assert_eq!(row.rescored_from.as_deref(), Some(source.as_str()), "{id}");
            assert_eq!(row.model_descriptor.as_ref(), Some(&descriptor), "{id}");
            assert_eq!(row.model_vault_descriptor, vault, "{id}");
            assert_eq!(
                row.model_descriptor_sha256,
                Some(sha256_hex(&id_text)),
                "{id}"
            );
        });
        Ok(())
    }

    /// One dense model, so anything else is a miss.
    fn one_model_table() -> anyhow::Result<ModelParamTable> {
        ModelParamTable::from_toml(r#""LFM2-700M" = 742"#).context("the test table must parse")
    }

    fn tally(pass: &PassTable<'_>) -> (Vec<(String, usize)>, usize, usize) {
        let unresolved = pass
            .unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (
            unresolved
                .named
                .iter()
                .map(|(k, v)| (k.clone(), v.rows))
                .collect(),
            unresolved.unparsed.rows,
            unresolved.scanned,
        )
    }

    fn gguf(repo: &str, file: &str) -> String {
        format!(
            r#"{{"type":"gguf_text","source":"huggingface","org":"someorg","repo_name":"{repo}","path":"{file}"}}"#
        )
    }

    /// `model_name` never resolves a row, however catalogued it looks.
    #[rstest]
    #[case::descriptor(None, Some(gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_0.gguf")), true)]
    #[case::descriptor_over_a_misleading_name(
        Some("some-alias-nobody-catalogued"),
        Some(gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_0.gguf")),
        true
    )]
    #[case::a_catalogued_name_alone(Some("LiquidAI/LFM2-700M-GGUF:Q4_0.gguf"), None, false)]
    fn only_the_descriptor_resolves(
        #[case] model_name: Option<&str>,
        #[case] model_descriptor: Option<String>,
        #[case] resolves: bool,
    ) -> anyhow::Result<()> {
        let table = one_model_table()?;
        let pass = PassTable::new(&table);
        // Named only to show it is ignored.
        let _ = model_name;

        assert_eq!(
            pass.resolve(4, model_descriptor.as_deref(), None).is_some(),
            resolves
        );
        Ok(())
    }

    #[rstest]
    #[case::the_public_name(r#""LFM2-700M" = 742"#, true)]
    #[case::the_vault_resource_id(r#""vault_0199aaaa-bbbb-7ccc-8ddd-000000000001" = 742"#, true)]
    #[case::neither(r#""K2-Horizon-3.7B" = 3700"#, false)]
    fn an_aliased_row_resolves_under_either_descriptor(
        #[case] toml: &str,
        #[case] resolves: bool,
    ) -> anyhow::Result<()> {
        let table = ModelParamTable::from_toml(toml).context("the test table must parse")?;
        let pass = PassTable::new(&table);
        let alias = gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_0.gguf");
        let reported = r#"{"type":"gguf_text","source":"vault_url","url":"/vault/resources/vault_0199aaaa-bbbb-7ccc-8ddd-000000000001/content","principal_id":"org_1"}"#;

        assert_eq!(
            pass.resolve(2, Some(&alias), Some(reported)).is_some(),
            resolves
        );
        Ok(())
    }

    /// Rows, not calls; keys are normalized names, so a model's variants share
    /// one line.
    #[test]
    fn misses_are_tallied_per_normalized_model_in_rows() -> anyhow::Result<()> {
        let table = one_model_table()?;
        let pass = PassTable::new(&table);

        let k2_q4 = gguf("K2-Horizon-3.7B-GGUF", "K2-Horizon-3.7B-Q4_K_M.gguf");
        let k2_q8 = gguf("K2-Horizon-3.7B", "K2-Horizon-3.7B-Q8_0.gguf");
        let minicpm = gguf("MiniCPM5-2B-GGUF", "MiniCPM5-2B-Q4_K_M.gguf");
        assert!(pass.resolve(2, Some(&k2_q4), None).is_none());
        assert!(pass.resolve(3, Some(&k2_q8), None).is_none());
        assert!(pass.resolve(1, Some(&minicpm), None).is_none());
        // A submission that derived no metrics has no row to report.
        assert!(pass.resolve(0, Some(&k2_q4), None).is_none());

        assert_eq!(
            tally(&pass),
            (
                vec![
                    ("k2-horizon-3.7b".to_string(), 5),
                    ("minicpm5-2b".to_string(), 1),
                ],
                0,
                6
            )
        );
        Ok(())
    }

    /// An unparseable descriptor and a missing one are different problems with
    /// different fixes.
    #[test]
    fn rows_with_no_parseable_descriptor_count_apart() -> anyhow::Result<()> {
        let table = one_model_table()?;
        let pass = PassTable::new(&table);

        assert!(
            pass.resolve(2, Some(r#"{"partner":"Nobody-Knows"}"#), None)
                .is_none()
        );
        assert!(pass.resolve(1, None, None).is_none());

        assert_eq!(tally(&pass), (vec![], 2, 3));
        Ok(())
    }

    /// Its rows still count: that is exactly when the columns fall back.
    #[test]
    fn an_empty_table_misses_everything() -> anyhow::Result<()> {
        let table = ModelParamTable::empty();
        let pass = PassTable::new(&table);

        let descriptor = gguf("LFM2-700M-GGUF", "LFM2-700M-Q4_0.gguf");
        assert!(pass.resolve(2, Some(&descriptor), None).is_none());

        assert_eq!(tally(&pass), (vec![("lfm2-700m".to_string(), 2)], 0, 2));
        Ok(())
    }

    // ── slow worker: parking ──────────────────────────────────────────────

    /// Local stores and an unreachable scorer. The tests' poison body fails to
    /// parse before the scorer is called; tests/scoring.rs covers scorer failures.
    fn parking_test_config(dir: &std::path::Path) -> anyhow::Result<CommitConfig> {
        std::fs::create_dir_all(dir.join("benchmarks"))?;
        std::fs::write(
            dir.join("benchmarks/prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )?;
        Ok(CommitConfig {
            evals_server_url: "http://127.0.0.1:1".to_string(),
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..CommitConfig::default()
        })
    }

    async fn failed_attempts(stores: &Stores, job_id: &JobId) -> anyhow::Result<Option<u64>> {
        Ok(stores
            .submissions
            .read_queue(ScoreQueueStage::Failures, job_id)
            .await?
            .and_then(|f| f["attempts"].as_u64()))
    }

    /// A `to_do` job that fails every pass is retried up to the bound, then
    /// parked: it still reads as scoring and has a record, but no pass retries it.
    #[tokio::test]
    async fn an_eval_that_never_scores_is_parked() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = parking_test_config(dir.path())?;
        let stores = build_local_fs_stores(&config)?;
        let poison = JobId::new_unchecked("poison");
        let body = json!({ "benchmark_id": "eval_test", "client_id": "c1" });
        stores
            .submissions
            .enqueue(ScoreQueueStage::ToDo, &poison, &body)
            .await?;
        let pass = || async { run_score_eval(&config, build_local_fs_stores(&config)?).await };

        futures::TryStreamExt::try_for_each(
            futures::stream::iter((1..MAX_SCORE_ATTEMPTS).map(anyhow::Ok)),
            |_| pass(),
        )
        .await?;
        assert_eq!(
            failed_attempts(&stores, &poison).await?,
            Some(MAX_SCORE_ATTEMPTS - 1)
        );
        assert!(
            stores
                .submissions
                .read_queue(ScoreQueueStage::ToDo, &poison)
                .await?
                .is_some(),
            "retried while under the bound"
        );

        pass().await?;
        let submissions = &stores.submissions;
        assert!(
            submissions
                .read_queue(ScoreQueueStage::ToDo, &poison)
                .await?
                .is_none()
        );
        assert_eq!(
            submissions
                .read_queue(ScoreQueueStage::Parked, &poison)
                .await?,
            Some(body)
        );
        assert_eq!(
            submissions
                .list_queue(ScoreQueueStage::Parked, NonZeroUsize::MIN)
                .await?,
            vec![poison.clone()]
        );
        let failures = submissions
            .read_queue(ScoreQueueStage::Failures, &poison)
            .await?
            .context("the failures entry stays beside the parked job")?;
        assert_eq!(failures["attempts"], MAX_SCORE_ATTEMPTS);
        assert!(failures["error"].as_str().is_some_and(|e| !e.is_empty()));
        assert!(submissions.has_record(&poison).await?);
        let found = submissions
            .find_job(&poison)
            .await?
            .context("a parked job is found")?;
        assert_eq!(found.state, JobState::Scoring);

        pass().await?;
        assert_eq!(
            failed_attempts(&stores, &poison).await?,
            Some(MAX_SCORE_ATTEMPTS),
            "a parked job is not retried"
        );
        Ok(())
    }

    /// A job that gets through clears the failures it counted.
    #[tokio::test]
    async fn an_eval_that_scores_clears_its_failures() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = parking_test_config(dir.path())?;
        let stores = build_local_fs_stores(&config)?;
        let job_id = JobId::new_unchecked("staged");
        let body = json!({ "benchmark_id": "eval_test", "client_id": "c1" });
        let submissions = &stores.submissions;
        submissions
            .enqueue(ScoreQueueStage::ToDo, &job_id, &body)
            .await?;
        // Already staged, so the pass drops its `to_do` entry without a call.
        submissions
            .enqueue(
                ScoreQueueStage::ToFinalize,
                &job_id,
                &json!({ "submission": body, "score": {} }),
            )
            .await?;
        submissions
            .enqueue(
                ScoreQueueStage::Failures,
                &job_id,
                &json!({ "attempts": 3, "error": "earlier" }),
            )
            .await?;

        run_score_eval(&config, build_local_fs_stores(&config)?).await?;
        assert_eq!(failed_attempts(&stores, &job_id).await?, None);
        Ok(())
    }
}
