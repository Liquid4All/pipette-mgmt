//! Response helpers for the app's tests.

use anyhow::Context;
use axum::body::Body;
use axum::http::header::LOCATION;

/// The response body as text.
pub(crate) async fn body_text(resp: axum::http::Response<Body>) -> anyhow::Result<String> {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    Ok(String::from_utf8(bytes.to_vec())?)
}

/// The `Location` a redirect names.
pub(crate) fn location(resp: &axum::http::Response<Body>) -> anyhow::Result<String> {
    Ok(resp
        .headers()
        .get(LOCATION)
        .context("a redirect carries a Location")?
        .to_str()?
        .to_string())
}
