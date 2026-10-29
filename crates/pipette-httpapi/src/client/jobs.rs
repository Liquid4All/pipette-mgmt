//! A device's reads of its own jobs: `GET /jobs/{job_id}` and
//! `GET /jobs/{job_id}/eval-sample-results`.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;

use pipette_pipeline::submission::Submission;
use pipette_plan_types::{
    BenchmarkType, ChatMessage, EvalSampleResult, FailedJob, JobId, JobRead, JobStatus,
    SubmittedJob,
};
use pipette_store::JobState;

use crate::auth::AuthenticatedClient;
use crate::error::AppError;
use crate::extract::{ApiPath, Parsed};
use crate::state::AppState;

// GET /jobs/{job_id}
pub async fn get_job(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    ApiPath(Parsed(job_id)): ApiPath<Parsed<JobId>>,
) -> Result<Json<JobRead>, AppError> {
    let record = state
        .submission_store
        .find_job(&job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to find the job record: {e:#}")))?
        .ok_or_else(|| AppError::NotFound("job not found".into()))?;

    // Stored bodies without a `message_type` tag (~20k in prod) parse as success.
    let submission = pipette_pipeline::submission::parse_stored_submission(&record.body)
        .map_err(|e| AppError::Internal(format!("malformed submission on disk: {e}")))?;

    // 404 on another client's job so its existence does not leak.
    let owner = match &submission {
        Submission::Success(s) => &s.client_id,
        Submission::Failure(f) => &f.client_id,
    };
    if owner.as_str() != client.client_id.as_str() {
        tracing::warn!("refused: the job belongs to another client");
        return Err(AppError::NotFound("job not found".into()));
    }

    match submission {
        Submission::Failure(f) => Ok(Json(JobRead::Failed(failed_job(*f)))),
        Submission::Success(s) => Ok(Json(JobRead::Submitted(
            submitted_job(&state, record.state, *s, &job_id).await?,
        ))),
    }
}

fn failed_job(f: pipette_pipeline::submission::FailureSubmission) -> FailedJob {
    let pipette_pipeline::submission::FailureSubmission {
        wire: input,
        job_id,
        submitted_at,
        benchmark_type,
        ..
    } = f;
    let pipette_plan_types::FailureInput {
        benchmark_id,
        failure_reason,
        model_name,
        model_quant,
        runtime_name,
        runtime_version,
        client_version,
        ..
    } = input;
    FailedJob {
        job_id,
        benchmark_id,
        benchmark_type,
        status: JobStatus::Failed,
        submitted_at,
        failure_reason,
        model_name,
        model_quant,
        runtime_name,
        runtime_version,
        client_version,
    }
}

async fn submitted_job(
    state: &AppState,
    job_state: JobState,
    s: pipette_pipeline::submission::SuccessSubmission,
    job_id: &JobId,
) -> Result<SubmittedJob, AppError> {
    let mut scored_at = None;
    let mut score_runtime_version = None;
    let mut metrics = None;

    if job_state == JobState::Processed {
        // Only the `warehouse_read_days` window is scanned: an older job reads
        // `processed` with `metrics: null` rather than a whole-archive scan.
        if let Some(job_metrics) = state
            .warehouse_store
            .read_job_metrics(&s.wire.benchmark_id, &s.client_id, job_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to read the job metrics: {e:#}")))?
        {
            scored_at = Some(job_metrics.scored_at);
            score_runtime_version = job_metrics.score_runtime_version;
            metrics = Some(job_metrics.metrics);
        }
    }

    let pipette_pipeline::submission::SuccessSubmission {
        wire: input,
        job_id: response_job_id,
        submitted_at,
        benchmark_type,
        ..
    } = s;
    Ok(SubmittedJob {
        job_id: response_job_id,
        benchmark_id: input.benchmark_id,
        benchmark_type,
        status: job_status(job_state),
        submitted_at,
        scored_at,
        score_runtime_version,
        metrics,
    })
}

fn job_status(state: JobState) -> JobStatus {
    match state {
        JobState::Incoming => JobStatus::Incoming,
        JobState::Scoring => JobStatus::Scoring,
        JobState::Processed => JobStatus::Processed,
    }
}

// GET /jobs/{job_id}/eval-sample-results
pub async fn get_eval_sample_results(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    ApiPath(Parsed(job_id)): ApiPath<Parsed<JobId>>,
) -> Result<impl IntoResponse, AppError> {
    let record = state
        .submission_store
        .find_job(&job_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to find the job record before its eval sample results: {e:#}"
            ))
        })?
        .ok_or_else(|| AppError::NotFound("job not found".into()))?;

    if record
        .body
        .get("client_id")
        .and_then(serde_json::Value::as_str)
        != Some(client.client_id.as_str())
    {
        tracing::warn!("refused: the job belongs to another client");
        return Err(AppError::NotFound("job not found".into()));
    }

    if record.state != JobState::Processed {
        return Err(AppError::NotFound("job not found".into()));
    }

    if !matches!(
        record
            .body
            .get("benchmark_type")
            .and_then(|t| BenchmarkType::deserialize(t).ok()),
        Some(BenchmarkType::Eval | BenchmarkType::VlmEval | BenchmarkType::AsrEval)
    ) {
        return Err(AppError::NotFound("job not found".into()));
    }

    // A failure is archived with no sample results, so the missing file below
    // is a fault only for a success.
    if record
        .body
        .get("message_type")
        .and_then(serde_json::Value::as_str)
        == Some("failure")
    {
        return Err(AppError::NotFound("job not found".into()));
    }

    let rows = state
        .eval_sample_result_store
        .read(&job_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read the eval sample results: {e:#}")))?
        .ok_or_else(|| {
            AppError::Internal("no eval sample results found for a processed job".into())
        })?;

    let result = rows
        .into_iter()
        .map(|r| {
            let messages: Vec<ChatMessage> = serde_json::from_str(&r.messages).map_err(|e| {
                AppError::Internal(format!(
                    "malformed messages JSON for sample '{}': {e}",
                    r.id
                ))
            })?;
            let measures = r
                .measures
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| {
                    AppError::Internal(format!(
                        "malformed measures JSON for sample '{}': {e}",
                        r.id
                    ))
                })?;
            Ok(EvalSampleResult {
                id: r.id,
                messages,
                completion: r.completion,
                is_correct: r.is_correct,
                failed: r.failed,
                // A failure sent with `stop_detail` alone stores no `failed_reason`.
                failed_reason: if r.failed {
                    r.failed_reason.or(r.stop_detail)
                } else {
                    None
                },
                measures,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(Json(result))
}
