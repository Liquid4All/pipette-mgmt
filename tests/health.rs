mod helpers;

use axum::http::StatusCode;

use helpers::{body_json, make_state, setup_benchmarks, unauthed_get};

#[tokio::test]
async fn test_health() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/health").await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "ok");
    Ok(())
}
