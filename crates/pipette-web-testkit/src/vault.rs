//! The vault as a client reaches it: each file uploaded by its digest, then the
//! files described as the plan variant they make up.

use axum::body::Body;
use serde_json::{Value, json};
use sha2::Digest;

use pipette_httpapi::state::AppState;

use crate::request::{body_json, raw_request};
use crate::user_auth::{UserSigner, signed_json, user_auth_headers};

/// A file every vault test uploads: small, and a gguf by its first bytes, so a
/// `gguf_text` upload takes it.
pub const VAULT_GGUF: &[u8] = b"GGUF\x03\x00\x00\x00private weights, not really";

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Where `principal`'s file with `sha256` is uploaded.
pub fn blob_uri(principal: &str, sha256: &str) -> String {
    format!("/api/v1/vault/blobs/{principal}/{sha256}")
}

/// `PUT` `bytes` for `principal` under `sha256`, signed as `signer`. The
/// digest is the caller's, so a test can send one the bytes do not hash to.
pub async fn put_vault_blob(
    state: &AppState,
    signer: &UserSigner,
    principal: &str,
    sha256: &str,
    bytes: &[u8],
) -> anyhow::Result<axum::http::Response<Body>> {
    let uri = blob_uri(principal, sha256);
    let (key, user_id, key_id) = signer;
    let headers = user_auth_headers(key, user_id, key_id, "PUT", &uri)?;
    raw_request(
        state,
        "PUT",
        &uri,
        &headers,
        Some("application/octet-stream"),
        bytes.to_vec(),
    )
    .await
}

/// `HEAD` on `principal`'s file with `sha256`, signed as `signer`.
pub async fn head_vault_blob(
    state: &AppState,
    signer: &UserSigner,
    principal: &str,
    sha256: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let uri = blob_uri(principal, sha256);
    let (key, user_id, key_id) = signer;
    let headers = user_auth_headers(key, user_id, key_id, "HEAD", &uri)?;
    raw_request(state, "HEAD", &uri, &headers, None, Body::empty()).await
}

/// The body describing `sha256` as a `gguf_text` model, for `owner` when named.
pub fn gguf_text_upload(owner: Option<&str>, sha256: &str) -> Value {
    let mut body = json!({
        "name": "acme private checkpoint",
        "type": "gguf_text",
        "model": {"filename": "acme-checkpoint-Q4_K_M.gguf", "sha256": sha256},
    });
    if let (Some(owner), Some(fields)) = (owner, body.as_object_mut()) {
        fields.insert("owner".into(), json!(owner));
    }
    body
}

/// `POST /api/v1/vault/resources` with `body`, signed as `signer`.
pub async fn describe_vault_upload(
    state: &AppState,
    signer: &UserSigner,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    signed_json(state, signer, "POST", "/api/v1/vault/resources", body).await
}

/// `PUT` `bytes` for `principal` under their own digest, signed as `signer`,
/// failing the test on a refusal. Returns the digest.
pub async fn upload_vault_blob(
    state: &AppState,
    signer: &UserSigner,
    principal: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let sha256 = sha256_hex(bytes);
    let resp = put_vault_blob(state, signer, principal, &sha256, bytes).await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "uploading the file failed with {}",
        resp.status()
    );
    Ok(sha256)
}

/// Describe the file under `sha256` as a `gguf_text` model for `owner` (the
/// signer when `None`), failing the test on a refusal. Returns the minted id.
pub async fn describe_gguf_text_ok(
    state: &AppState,
    signer: &UserSigner,
    owner: Option<&str>,
    sha256: &str,
) -> anyhow::Result<String> {
    let resp = describe_vault_upload(state, signer, &gguf_text_upload(owner, sha256)).await?;
    anyhow::ensure!(
        resp.status() == axum::http::StatusCode::CREATED,
        "describing the upload failed with {}: {}",
        resp.status(),
        body_json(resp).await.unwrap_or_default()
    );
    let answer = body_json(resp).await?;
    answer
        .pointer("/resources/0/resource_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the answer names no resource: {answer}"))
}

/// Upload `bytes` for `owner` (the signer when `None`) and describe them as a
/// `gguf_text` model, failing the test on any refusal. Returns the minted id.
pub async fn upload_gguf_text(
    state: &AppState,
    signer: &UserSigner,
    owner: Option<&str>,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let principal = owner.map_or_else(|| signer.1.to_string(), str::to_string);
    let sha256 = upload_vault_blob(state, signer, &principal, bytes).await?;
    describe_gguf_text_ok(state, signer, owner, &sha256).await
}
