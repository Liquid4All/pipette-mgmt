//! `GET /health`: a fixed liveness marker.

use axum::Json;
use axum::response::IntoResponse;

use pipette_plan_types::HealthResponse;

pub async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}
