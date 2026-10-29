//! The benchmark catalog: `GET /benchmarks`, `GET /benchmarks/{benchmark_id}`
//! and their `/api/v2` twins, each answered with an `ETag`.

pub mod submissions;

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use pipette_core::benchmark::Benchmark;
use pipette_plan_types::{
    BenchmarkDefinition, BenchmarkId, CatalogEntry, CatalogQuery, EvalBenchmark, EvalSample,
    NonEmptyString, OrgId,
};

use crate::auth::CatalogReader;
use crate::error::AppError;
use crate::etag;
use crate::extract::{ApiPath, ApiQuery};
use crate::state::AppState;

fn json_with_etag(headers: &HeaderMap, value: &impl Serialize) -> Result<Response, AppError> {
    let body = serde_json::to_vec(value)
        .map_err(|e| AppError::Internal(format!("failed to serialize the response: {e}")))?;
    let etag = etag::of(&body);
    let etag_header = HeaderValue::from_str(&etag)
        .map_err(|e| AppError::Internal(format!("failed to build etag header: {e}")))?;
    if etag::matched(headers, &etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, etag_header);
        return Ok(response);
    }
    let mut response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    response.headers_mut().insert(header::ETAG, etag_header);
    Ok(response)
}
/// The public catalog wins on an id both hold. The write side refuses such an
/// entry, so this only drops a file planted behind the routes' back.
async fn private_catalog(
    state: &AppState,
    reader: &CatalogReader,
    public: &HashMap<BenchmarkId, Benchmark>,
) -> Result<HashMap<BenchmarkId, Benchmark>, AppError> {
    let Some(org_id) = reader.org_id(state).await? else {
        return Ok(HashMap::new());
    };
    let mut private = state
        .catalog_store
        .load_org_catalog(&org_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to load the organization's benchmark catalog: {e:#}"
            ))
        })?;
    private.retain(|benchmark_id, _| {
        let shadowed = public.contains_key(benchmark_id);
        if shadowed {
            tracing::warn!(
                %org_id, %benchmark_id,
                "a private benchmark takes a public id and is not served"
            );
        }
        !shadowed
    });
    Ok(private)
}

/// The entry `benchmark_id` names in `org_id`'s own catalog, read from the
/// store now; `None` when there is none, or no organization to ask.
pub(crate) async fn org_benchmark(
    state: &AppState,
    org_id: Option<OrgId>,
    benchmark_id: &BenchmarkId,
) -> Result<Option<Benchmark>, AppError> {
    let Some(org_id) = org_id else {
        return Ok(None);
    };
    state
        .catalog_store
        .get_org_benchmark(&org_id, benchmark_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to read the organization's benchmark: {e:#}"
            ))
        })
}

/// The public entry, else the reader's organization's. An outsider gets the
/// same `404` as an unknown id, so a private id discloses nothing.
async fn lookup_benchmark(
    state: &AppState,
    reader: &CatalogReader,
    benchmark_id: &BenchmarkId,
) -> Result<Benchmark, AppError> {
    let public = state.catalog_cache.get().await.map_err(|e| {
        AppError::Internal(format!(
            "failed to load the benchmark catalog to look up one benchmark: {e:#}"
        ))
    })?;
    if let Some(benchmark) = public.get(benchmark_id) {
        return Ok(benchmark.clone());
    }
    org_benchmark(state, reader.org_id(state).await?, benchmark_id)
        .await?
        .ok_or_else(|| AppError::NotFound("benchmark not found".into()))
}

/// The entries `reader` is served, of the `type` the query names if any,
/// sorted by id: what both catalog listings serve.
async fn list_catalog(
    state: &AppState,
    reader: &CatalogReader,
    query: &CatalogQuery,
) -> Result<Vec<Benchmark>, AppError> {
    let catalog = state.catalog_cache.get().await.map_err(|e| {
        AppError::Internal(format!(
            "failed to load the benchmark catalog to list benchmarks: {e:#}"
        ))
    })?;
    let private = private_catalog(state, reader, &catalog).await?;
    let mut benchmarks: Vec<Benchmark> = catalog
        .values()
        .cloned()
        .chain(private.into_values())
        .filter(|b| {
            query
                .type_filter
                .is_none_or(|wanted| b.benchmark_type() == wanted)
        })
        .collect();
    benchmarks.sort_by(|a, b| a.benchmark_id().cmp(b.benchmark_id()));
    Ok(benchmarks)
}

pub async fn list_benchmarks(
    State(state): State<AppState>,
    reader: CatalogReader,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<CatalogQuery>,
) -> Result<Response, AppError> {
    let entries: Vec<CatalogEntry> = list_catalog(&state, &reader, &query)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    json_with_etag(&headers, &entries)
}

// GET /api/v2/benchmarks
pub async fn list_benchmarks_v2(
    State(state): State<AppState>,
    reader: CatalogReader,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<CatalogQuery>,
) -> Result<Response, AppError> {
    let entries: Vec<_> = list_catalog(&state, &reader, &query)
        .await?
        .iter()
        .map(Benchmark::entry)
        .collect();
    json_with_etag(&headers, &entries)
}

// GET /benchmarks/{benchmark_id}
pub async fn get_benchmark(
    State(state): State<AppState>,
    reader: CatalogReader,
    headers: HeaderMap,
    ApiPath(benchmark_id): ApiPath<BenchmarkId>,
) -> Result<Response, AppError> {
    let benchmark = lookup_benchmark(&state, &reader, &benchmark_id).await?;

    let mut entry = CatalogEntry::from(benchmark);
    if let Some(dataset) = entry.definition.dataset_mut() {
        let url = crate::client::datasets::dataset_url(&benchmark_id);
        dataset.url = Some(
            NonEmptyString::try_new(url)
                .map_err(|e| AppError::Internal(format!("the corpus url is not a string: {e}")))?,
        );
    }

    // A text eval's samples come from the evals server inline. A vlm_eval's
    // and an asr_eval's are their corpus, downloaded from `/datasets/{id}`
    // like any other: inline, each image would cross the wire once per
    // question that shows it, and an ASR set is gigabytes of audio.
    let samples_source = match &entry.definition {
        BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_eval_id,
            parameter_dataset_name,
            ..
        }) => Some((parameter_eval_id.as_str(), parameter_dataset_name.as_str())),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlmEval(_)
        | BenchmarkDefinition::AsrEval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => None,
    };
    if let Some((eval_id, dataset_name)) = samples_source {
        let samples = pipette_pipeline::scoring_service::fetch_samples(
            &state.http_client,
            &state.config.evals_server_url,
            eval_id,
            dataset_name,
        )
        .await
        .map_err(|e| AppError::BadGateway(format!("failed to fetch the eval samples: {e}")))?;
        entry.samples = Some(
            samples
                .samples
                .into_iter()
                .map(|sample| EvalSample {
                    id: sample.id.into_inner(),
                    messages: sample.messages,
                    chat_template_kwargs: sample.chat_template_kwargs,
                })
                .collect(),
        );
    }

    json_with_etag(&headers, &entry)
}

// GET /api/v2/benchmarks/{benchmark_id}
//
// The entry alone: no eval samples and no corpus, so no upstream is called.
pub async fn get_benchmark_v2(
    State(state): State<AppState>,
    reader: CatalogReader,
    headers: HeaderMap,
    ApiPath(benchmark_id): ApiPath<BenchmarkId>,
) -> Result<Response, AppError> {
    let entry = lookup_benchmark(&state, &reader, &benchmark_id)
        .await?
        .entry();
    json_with_etag(&headers, &entry)
}
