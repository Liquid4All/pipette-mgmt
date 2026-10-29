//! Strong `ETag`s over a body's bytes, and the `If-None-Match` that names one.

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};

/// The quoted SHA-256 of `body`.
pub(crate) fn of(body: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Sha256::digest(body)))
}

/// Whether the request's `If-None-Match` names `etag`, weakly or not, or `*`.
pub(crate) fn matched(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers.get(header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
    })
}
