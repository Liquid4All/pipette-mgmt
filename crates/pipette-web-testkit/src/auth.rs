//! Signing a request as a client, and registering the client that signs it.
//!
//! Each registration helper states a different principal, so the one a test
//! reaches for says what it assumes about authorization.

use axum::body::Body;
use axum::http::Request;
use ed25519_dalek::{Signature, Signer, SigningKey};
use getrandom::SysRng;
use getrandom::rand_core::TryRng;
use serde_json::json;
use tower::ServiceExt;

use pipette_core::client_permission::ClientPermissionSet;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, ClientOwner, ClientPermission, OrgId};

use crate::app::{make_state, setup_benchmarks, test_app};
use crate::request::body_json;
use crate::user_auth::TEST_AUTHORITY;

/// Auth headers signing the given request at the current time.
pub fn auth_headers(
    signing_key: &SigningKey,
    client_id: &ClientId,
    method: &str,
    path: &str,
) -> Result<Vec<(&'static str, String)>, getrandom::Error> {
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    auth_headers_at(signing_key, client_id, method, path, timestamp)
}

/// Auth headers whose signature covers only the timestamp — the payload the
/// server accepts while `accept_legacy_signatures` is set.
pub fn legacy_auth_headers(
    signing_key: &SigningKey,
    client_id: &ClientId,
) -> Vec<(&'static str, String)> {
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let signature = signing_key.sign(timestamp.as_bytes());
    signed_headers(client_id, timestamp, signature)
}

/// Auth headers signing the given request at a caller-chosen timestamp, over
/// the `v3` payload for, and sent to, [`TEST_AUTHORITY`].
///
/// Not shared with `src/auth.rs` on purpose: an independent copy of the wire
/// format makes a server-side change fail a test instead of being mirrored.
pub fn auth_headers_at(
    signing_key: &SigningKey,
    client_id: &ClientId,
    method: &str,
    path: &str,
    timestamp: String,
) -> Result<Vec<(&'static str, String)>, getrandom::Error> {
    let nonce = nonce()?;
    let signature = signing_key.sign(
        format!("v3\n{method}\n{TEST_AUTHORITY}\n{path}\n{timestamp}\n{client_id}\n{nonce}")
            .as_bytes(),
    );
    let mut headers = signed_headers(client_id, timestamp, signature);
    headers.push(("X-Nonce", nonce));
    headers.push(("Host", TEST_AUTHORITY.to_string()));
    Ok(headers)
}

/// A nonce unique to one request, which is what makes its signature unique.
pub(crate) fn nonce() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    SysRng.try_fill_bytes(&mut bytes)?;
    Ok(hex::encode(bytes))
}

pub use pipette_httpapi::auth::generate_signing_key as signing_key;

/// The header envelope every payload shares. The timestamp-only one covers no
/// nonce, so the others' signers add `X-Nonce`.
fn signed_headers(
    client_id: &ClientId,
    timestamp: String,
    signature: Signature,
) -> Vec<(&'static str, String)> {
    vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(signature.to_bytes())),
    ]
}

/// Register a client and leave it `pending` (unapproved).
pub async fn register_pending(state: &AppState) -> anyhow::Result<(SigningKey, ClientId)> {
    let signing_key = signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let app = test_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/clients/register")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json!({
                    "public_key": pk_hex,
                    "organization": "test-org",
                    "client_details": "test",
                    "contact_email": "t@t.com"
                }))?))?,
        )
        .await?;

    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body.get("client_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing client_id in register response"))?,
    )?;
    Ok((signing_key, client_id))
}

/// Register a client, approve it, and grant it the whole client vocabulary —
/// what a lab device holds.
///
/// Approval alone authorizes nothing: without `perform_work` the queue routes
/// refuse it. Permission tests state their set with
/// [`register_approve_and_grant`] instead.
pub async fn register_and_approve(state: &AppState) -> anyhow::Result<(SigningKey, ClientId)> {
    register_approve_and_grant(
        state,
        &[
            ClientPermission::PerformWork,
            ClientPermission::SubmitResults,
        ],
    )
    .await
}

/// Register a client, approve it, and grant it exactly `permissions`, replacing
/// what registration seeded. For the tests whose subject is the permission
/// check: pass `&[]` for a client an operator vetted and granted nothing.
pub async fn register_approve_and_grant(
    state: &AppState,
    permissions: &[ClientPermission],
) -> anyhow::Result<(SigningKey, ClientId)> {
    let signing_key = signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());
    let client_id = register_and_approve_public_key(state, &pk_hex).await?;
    grant(state, &client_id, permissions).await?;
    Ok((signing_key, client_id))
}

/// [`own_client`] for the common case: a device the organization holds
/// directly.
pub async fn own_client_by_org(
    state: &AppState,
    client_id: &ClientId,
    org_id: &OrgId,
) -> anyhow::Result<()> {
    own_client(
        state,
        client_id,
        ClientOwner::Org {
            org_id: org_id.clone(),
        },
    )
    .await
}

/// Set `client_id`'s owner through the store, as `clients owner set` would.
/// For the tests whose subject is what ownership reaches, not the route that
/// writes it.
pub async fn own_client(
    state: &AppState,
    client_id: &ClientId,
    owner: ClientOwner,
) -> anyhow::Result<()> {
    state
        .client_store
        .get_client(client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
    state.org_store.set_ownership(client_id, Some(owner)).await
}

/// Replace `client_id`'s stored permission set with exactly `permissions`.
pub async fn grant(
    state: &AppState,
    client_id: &ClientId,
    permissions: &[ClientPermission],
) -> anyhow::Result<()> {
    let set: ClientPermissionSet = permissions.iter().copied().collect();
    state
        .client_store
        .put_client_permissions(client_id, &set)
        .await?;
    Ok(())
}

/// Register and approve a caller-supplied public key, for the tests whose
/// subject is the key itself rather than a client holding one.
///
/// Leaves the set registration seeded — `submit_results` and nothing else.
pub async fn register_and_approve_public_key(
    state: &AppState,
    pk_hex: &str,
) -> anyhow::Result<ClientId> {
    let app = test_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/clients/register")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json!({
                    "public_key": pk_hex,
                    "organization": "test-org",
                    "client_details": "test",
                    "contact_email": "t@t.com"
                }))?))?,
        )
        .await?;

    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body.get("client_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing client_id in register response"))?,
    )?;

    let mut client = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found after registration"))?;
    client.status = pipette_core::client::ClientStatus::Approved;
    state.client_store.put_client(&client).await?;

    Ok(client_id)
}

/// An `AppState` over `dir` seeded with benchmarks, and a registered, approved,
/// fully granted client — the setup most route tests open with.
///
/// The caller keeps `dir`'s guard bound for the test, since dropping it deletes
/// the store. Not returned from here: `tempfile` would become a shipped dependency.
pub async fn approved_client(
    dir: &std::path::Path,
) -> anyhow::Result<(AppState, SigningKey, ClientId)> {
    setup_benchmarks(dir)?;
    let state = make_state(dir).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    Ok((state, key, client_id))
}
