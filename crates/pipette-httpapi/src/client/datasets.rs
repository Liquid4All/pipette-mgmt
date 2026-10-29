//! A benchmark's corpus, streamed from the cache only on a fresh probe `ok`.
//! No request contacts an upstream.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;

use pipette_core::dataset::{ProbeState, ServeRefusal};
use pipette_plan_types::{BenchmarkId, ErrorBody};

use crate::auth::{WorkingClient, client_catalog_org};
use crate::error::AppError;
use crate::extract::ApiPath;
use crate::state::AppState;

/// One probe tick.
const RETRY_AFTER_SECS: &str = "60";

pub fn dataset_url(benchmark_id: &BenchmarkId) -> String {
    format!("/datasets/{benchmark_id}")
}

/// Fixed, topology-free message; the detail goes to the log.
fn refuse(status: StatusCode, message: &'static str) -> Response {
    let mut response = (
        status,
        axum::Json(ErrorBody {
            error: message.to_string(),
        }),
    )
        .into_response();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_static(RETRY_AFTER_SECS),
        );
    }
    response
}

// GET /datasets/{benchmark_id}
pub async fn get_dataset(
    State(state): State<AppState>,
    WorkingClient(client): WorkingClient,
    ApiPath(benchmark_id): ApiPath<BenchmarkId>,
) -> Result<Response, AppError> {
    let catalog = state.catalog_cache.get().await.map_err(|e| {
        AppError::Internal(format!(
            "failed to load the benchmark catalog to serve a dataset: {e:#}"
        ))
    })?;
    // Reached as the definition is: public first, then the client's org.
    let benchmark = match catalog.get(&benchmark_id) {
        Some(benchmark) => benchmark.clone(),
        None => {
            let org_id = client_catalog_org(&state, &client).await?;
            crate::client::benchmarks::org_benchmark(&state, org_id, &benchmark_id)
                .await?
                .ok_or_else(|| AppError::NotFound("benchmark not found".into()))?
        }
    };
    let dataset = benchmark
        .dataset()
        .ok_or_else(|| AppError::NotFound("benchmark has no dataset".into()))?;
    let digest = dataset.digest.as_ref();

    let Some(artifact) = state
        .dataset_cache_store
        .get_artifact(&dataset.digest)
        .await
        .map_err(|e| {
            AppError::Internal(format!("failed to read dataset artifact metadata: {e:#}"))
        })?
    else {
        tracing::error!(
            event = "dataset_cache_missing",
            %benchmark_id, digest, client_id = %client.client_id,
            "dataset artifact not built; refusing to serve"
        );
        return Ok(refuse(StatusCode::SERVICE_UNAVAILABLE, "dataset not built"));
    };

    // The artifact under this digest must be the one this entry pins: an
    // entry that copied another's digest would otherwise be served that
    // corpus, judged by that corpus's probe.
    if !artifact.is_built_from(dataset) {
        tracing::error!(
            event = "dataset_pin_conflict",
            %benchmark_id, digest,
            pinned = %dataset.upstream.reference(),
            built_from = %artifact.upstream.reference(),
            "dataset artifact was built from another pin; refusing to serve"
        );
        return Ok(refuse(
            StatusCode::BAD_GATEWAY,
            "artifact does not match the pin",
        ));
    }

    // An asr_eval entry states how many samples its corpus holds: one that
    // disagrees would be downloaded and then refused on every device.
    if let Some(stated) = pipette_pipeline::dataset_cache::stated_samples(&benchmark)
        && let Err(e) = pipette_pipeline::dataset_cache::ensure_holds(artifact.samples, stated)
    {
        tracing::error!(
            event = "dataset_sample_count_mismatch",
            %benchmark_id, digest,
            error = format!("{e:#}"),
            "dataset artifact does not hold the entry's samples; refusing to serve"
        );
        return Ok(refuse(
            StatusCode::BAD_GATEWAY,
            "artifact does not hold the entry's samples",
        ));
    }

    let max_age = i64::try_from(state.config.dataset_probe_max_age_secs.get())
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .ok_or_else(|| {
            AppError::Internal("dataset_probe_max_age_secs is out of range".to_string())
        })?;
    match artifact.serveable(Utc::now(), max_age) {
        Ok(()) => {}
        Err(ServeRefusal::Stale { checked_at }) => {
            tracing::error!(
                event = "dataset_probe_stale",
                %benchmark_id, digest, %checked_at,
                "dataset probe verdict is stale; is the probe task running?"
            );
            return Ok(refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream check stale",
            ));
        }
        // The probe alerted on the transition; one line per refusal is enough
        // to correlate a client's failure with it.
        Err(ServeRefusal::Unavailable { since }) => {
            let probe = ProbeState::Unavailable;
            tracing::warn!(%benchmark_id, digest, %probe, %since, "refusing to serve dataset");
            return Ok(refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream unavailable",
            ));
        }
        Err(ServeRefusal::Broken {
            state: probe,
            since,
        }) => {
            tracing::warn!(%benchmark_id, digest, %probe, %since, "refusing to serve dataset");
            return Ok(refuse(
                StatusCode::BAD_GATEWAY,
                "upstream no longer backs the pin",
            ));
        }
    }

    let Some(body) = state
        .dataset_cache_store
        .open_tar(&dataset.digest)
        .await
        .map_err(|e| AppError::Internal(format!("failed to open dataset artifact: {e:#}")))?
    else {
        tracing::error!(
            event = "dataset_cache_missing",
            %benchmark_id, digest,
            "dataset metadata present but its tar is not; refusing to serve"
        );
        return Ok(refuse(StatusCode::SERVICE_UNAVAILABLE, "dataset not built"));
    };

    let etag = HeaderValue::from_str(&format!("\"{digest}\""))
        .map_err(|e| AppError::Internal(format!("dataset digest is not a header value: {e}")))?;
    let mut response = Body::from_stream(body.stream).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-tar"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.bytes));
    headers.insert(header::ETAG, etag);
    // The client keeps the corpus for the job, not beyond it.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
