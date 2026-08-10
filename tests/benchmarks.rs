mod helpers;

use axum::http::{StatusCode, header};

use helpers::{
    body_json, make_state, make_state_with_evals_url, setup_benchmarks,
    start_mock_evals_server_malformed_samples, unauthed_get, unauthed_get_with_headers,
};

#[tokio::test]
async fn test_unauthed_client_can_list_benchmarks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let benchmarks = body
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected benchmarks array"))?;
    assert_eq!(benchmarks.len(), 4);

    for bm in benchmarks {
        assert!(bm.get("benchmark_type").is_some());
        assert!(bm.get("type").is_none());
        assert!(bm.get("benchmark_id").is_some());
        assert!(bm.get("visibility").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn test_benchmark_list_honors_if_none_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag header"))?
        .to_str()?
        .to_string();

    let resp = unauthed_get_with_headers(
        &state,
        "/benchmarks",
        &[(header::IF_NONE_MATCH.as_str(), etag)],
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert!(resp.headers().get(header::ETAG).is_some());
    Ok(())
}

#[tokio::test]
async fn test_unauthed_client_can_get_benchmark() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/prefill_throughput_256").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(
        body["benchmark_id"].as_str(),
        Some("prefill_throughput_256")
    );
    assert_eq!(body["benchmark_type"].as_str(), Some("prefill_throughput"));
    assert!(body.get("visibility").is_none());
    Ok(())
}

#[tokio::test]
async fn test_get_benchmark_honors_if_none_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/prefill_throughput_256").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag header"))?
        .to_str()?
        .to_string();

    let resp = unauthed_get_with_headers(
        &state,
        "/benchmarks/prefill_throughput_256",
        &[(header::IF_NONE_MATCH.as_str(), etag)],
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert!(resp.headers().get(header::ETAG).is_some());
    Ok(())
}

#[tokio::test]
async fn test_get_benchmark_404_when_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/does_not_exist").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_eval_benchmark_returns_502_when_upstream_unreachable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/eval_test").await?;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    Ok(())
}

#[tokio::test]
async fn test_eval_benchmark_returns_502_on_malformed_upstream_response() -> anyhow::Result<()> {
    let evals_url = start_mock_evals_server_malformed_samples().await?;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals_url).await?;

    let resp = unauthed_get(&state, "/benchmarks/eval_test").await?;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    Ok(())
}
