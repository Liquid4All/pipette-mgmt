//! Driving the HTTP surface and reading what comes back. Everything routes
//! through [`crate::app::test_app`], so a request travels the binary's layers.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use pipette_plan_types::ClientId;

use pipette_httpapi::state::AppState;

use crate::app::test_app;
use crate::auth::auth_headers;
use crate::user_auth::{UserSigner, signed_json};

pub async fn body_json(resp: axum::http::Response<Body>) -> anyhow::Result<Value> {
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

/// Extract a string field from a JSON body, erroring (rather than
/// panicking) when it is missing or not a string — so tests can `?` it.
pub fn json_str<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing or non-string field {key:?}"))
}

/// A one-record write's answer split into the record and its `changed` flag,
/// so the record compares with a read of it.
pub fn changed_record(mut body: Value) -> anyhow::Result<(Value, bool)> {
    let changed = body
        .as_object_mut()
        .and_then(|fields| fields.remove("changed"))
        .and_then(|changed| changed.as_bool())
        .ok_or_else(|| anyhow::anyhow!("no boolean `changed` in {body}"))?;
    Ok((body, changed))
}

/// Send each `(body, changed, writes)` step to one write route in turn, and
/// check the answer's `changed` and the `writes` count after it. The answers,
/// in step order, are for the checks a route adds.
pub async fn check_write_steps(
    state: &AppState,
    signer: &UserSigner,
    (method, uri): (&str, &str),
    writes: impl Fn() -> usize,
    steps: impl IntoIterator<Item = (Value, bool, usize)>,
) -> anyhow::Result<Vec<Value>> {
    let mut answers = Vec::new();
    for (body, changed, expected) in steps {
        let resp = signed_json(state, signer, method, uri, &body).await?;
        let status = resp.status();
        anyhow::ensure!(status == StatusCode::OK, "{method} {uri} {body}: {status}");
        let answer = body_json(resp).await?;
        anyhow::ensure!(
            answer.get("changed") == Some(&Value::Bool(changed)),
            "{body}: expected changed {changed}, got {answer}"
        );
        let written = writes();
        anyhow::ensure!(
            written == expected,
            "{body}: {written} writes, expected {expected}"
        );
        answers.push(answer);
    }
    Ok(answers)
}

/// The `benchmark_id` of every row in a catalog listing body, sorted so a
/// comparison does not depend on the served order.
pub fn benchmark_ids(body: &Value) -> Vec<String> {
    let mut ids: Vec<String> = body
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["benchmark_id"].as_str().map(str::to_string))
        .collect();
    ids.sort();
    ids
}

/// Send a GET request with auth headers, return the response.
pub async fn authed_get(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "GET", uri)?;
    request_with_headers(state, "GET", uri, &headers).await
}

/// Send a GET request with pre-built auth headers, return the response.
pub async fn authed_get_with_headers(
    state: &AppState,
    uri: &str,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<Body>> {
    request_with_headers(state, "GET", uri, headers).await
}

/// Send an empty-bodied request with a caller-chosen method, URI, and
/// pre-built headers. Lets a test sign one request and send a different one.
pub async fn request_with_headers(
    state: &AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<Body>> {
    raw_request(state, method, uri, headers, None, Body::empty()).await
}

/// Send a JSON-bodied request with a caller-chosen method, URI, and headers.
pub async fn json_request(
    state: &AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let body = serde_json::to_string(body)?;
    raw_request(state, method, uri, headers, Some("application/json"), body).await
}

/// Send a request whose body and `Content-Type` the caller chooses (`None`
/// sends none). Sign `headers` over the whole `uri`: the signature covers the query.
pub async fn raw_request(
    state: &AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    content_type: Option<&str>,
    body: impl Into<Body>,
) -> anyhow::Result<axum::http::Response<Body>> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match content_type {
        Some(content_type) => builder.header("Content-Type", content_type),
        None => builder,
    };
    let req = headers
        .iter()
        .fold(builder, |req, (k, v)| req.header(*k, v));
    let app = test_app(state.clone());
    Ok(app.oneshot(req.body(body.into())?).await?)
}

/// Send an unauthenticated GET request, return the response.
pub async fn unauthed_get(
    state: &AppState,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let app = test_app(state.clone());
    Ok(app
        .oneshot(Request::builder().uri(uri).body(Body::empty())?)
        .await?)
}

/// Send an unauthenticated GET request with extra headers, return the response.
pub async fn unauthed_get_with_headers(
    state: &AppState,
    uri: &str,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<Body>> {
    request_with_headers(state, "GET", uri, headers).await
}

/// Send an unauthenticated POST request with JSON body, return the response.
pub async fn unauthed_post(
    state: &AppState,
    uri: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    json_request(state, "POST", uri, &[], body).await
}

/// Send an authenticated POST request with JSON body, return the response.
pub async fn authed_post(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "POST", uri)?;
    json_request(state, "POST", uri, &headers, body).await
}

/// Send an authenticated PUT request with an empty body, return the response.
pub async fn authed_put_empty(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "PUT", uri)?;
    request_with_headers(state, "PUT", uri, &headers).await
}

/// Send an authenticated PATCH request with JSON body, return the response.
pub async fn authed_patch(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "PATCH", uri)?;
    json_request(state, "PATCH", uri, &headers, body).await
}
