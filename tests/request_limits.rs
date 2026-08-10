mod helpers;

use axum::http::StatusCode;
use serde_json::json;

use helpers::{make_state, register_and_approve, setup_benchmarks, unauthed_post};
use pipette_mgmt::router::DEFAULT_BODY_LIMIT;

/// A body is buffered in full before it is parsed, so the ceiling has to stop
/// one before the other. `POST /clients/register` is the route where that
/// matters most — it serves unauthenticated callers — and the rejection reports
/// the size rather than blaming the caller's JSON.
#[tokio::test]
async fn test_oversized_body_is_rejected_before_parsing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Valid in every respect except size, so a rejection can only be the limit.
    let body = json!({
        "generate_key": true,
        "organization": "test-org",
        "client_details": "x".repeat(DEFAULT_BODY_LIMIT + 1),
        "contact_email": "t@t.com"
    });
    let resp = unauthed_post(&state, "/clients/register", &body).await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = helpers::body_json(resp).await?;
    assert_eq!(
        helpers::json_str(&body, "error")?,
        "request body is too large"
    );
    Ok(())
}

/// The submission routes raise the ceiling, so a body far past the router-wide
/// limit reaches the handler and is judged on its contents. Without the
/// per-route raise this would be a `413` and no submission carrying eval
/// completions could be sent at all.
#[tokio::test]
async fn test_submission_routes_take_bodies_over_the_default_limit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = json!({ "padding": "x".repeat(DEFAULT_BODY_LIMIT + 1) });
    let resp = helpers::authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;

    // A `400` is the submission failing validation — which is only reachable
    // once the body has been read whole.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}
