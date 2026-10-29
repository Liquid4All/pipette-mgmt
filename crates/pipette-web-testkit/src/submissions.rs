//! Benchmark submissions: a body that validates, and the route that takes it.

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use pipette_plan_types::ClientId;

use pipette_httpapi::state::AppState;

use crate::request::{authed_post, body_json, json_str};

/// A submission body that passes validation for `benchmark_id`. Callers that
/// echo a claim set `body["job_id"]` themselves.
pub fn valid_submission(benchmark_id: &str) -> Value {
    json!({
        "benchmark_id": benchmark_id,
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "model_params_total_millions": 1000,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "prefill_time_ms": 34.7
    })
}

/// Send a POST /benchmarks submission, assert it was accepted, and return the
/// job_id.
pub async fn submit_benchmark(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    submission: &Value,
) -> anyhow::Result<String> {
    let resp = authed_post(state, signing_key, client_id, "/benchmarks", submission).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    Ok(json_str(&body_json(resp).await?, "job_id")?.to_string())
}
