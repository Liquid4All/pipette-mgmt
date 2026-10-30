//! The error envelope on the refusals no handler writes: a path or a method no
//! route serves, and a path segment that does not parse.

use anyhow::Context;
use axum::http::{StatusCode, header};
use rstest::rstest;

use pipette_web_testkit::{
    approved_client, authed_put_empty, body_json, json_str, make_state, request_with_headers,
    setup_benchmarks, unauthed_get,
};

/// A path no route serves is a `404` in the envelope, not an empty body.
#[tokio::test]
async fn an_unrouted_path_is_a_404_in_the_envelope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/no-such-route").await?;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "error")?, "no such route");
    Ok(())
}

/// A method its path does not serve is a `405` in the envelope that still
/// names the methods the path does serve, on a plain route and on one that
/// wraps its methods in a body-limit layer of its own.
#[rstest]
#[case::health("POST", "/health", "GET")]
#[case::layered_route("DELETE", "/benchmarks", "POST")]
#[tokio::test]
async fn an_unserved_method_is_a_405_in_the_envelope(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] served: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = request_with_headers(&state, method, uri, &[]).await?;

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = resp
        .headers()
        .get(header::ALLOW)
        .context("a 405 names the methods the path serves")?
        .to_str()?
        .to_string();
    assert!(allow.contains(served), "{allow}");
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "error")?, "method not allowed");
    Ok(())
}

/// A benchmark id outside the id grammar is the caller's `400` in the envelope,
/// on a route that answers an unsigned caller.
#[rstest]
#[tokio::test]
async fn a_malformed_benchmark_id_is_a_400_in_the_envelope(
    #[values("/benchmarks/-x", "/api/v2/benchmarks/-x")] uri: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, uri).await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("benchmark id"), "{error}");
    Ok(())
}

/// A segment that is not UTF-8 once percent-decoded is a `400` in the envelope
/// naming the parameter, once the signature has admitted the caller.
#[tokio::test]
async fn an_undecodable_signed_segment_is_a_400_in_the_envelope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/%FF/heartbeat").await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("job_id"), "{error}");
    Ok(())
}
