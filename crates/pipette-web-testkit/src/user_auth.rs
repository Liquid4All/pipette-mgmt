//! Signing a request as a **user**, and creating the account that signs it.
//!
//! Names carry a `user_` prefix because [`super`] re-exports flat beside the
//! client helpers. Accounts are seeded through the store, as the host CLI does:
//! minting a pre-auth key over HTTP is itself under test.

use axum::body::Body;
use axum::http::Request;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use tower::ServiceExt;

use pipette_core::organization::Organization;
use pipette_core::user::User;
use pipette_core::user_permission::UserPermissionSet;
use pipette_core::user_preauth::UserPreauthParams;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    ContactEmail, NonEmptyTrimmedString, OrgId, OrgSlug, UserAccountType, UserId, UserKeyId,
    UserPermission, UserPreauthUsage, UserStatus,
};

use crate::app::test_app;
use crate::request::body_json;

/// Auth headers signing `method path` as `user_id` with `key_id`.
///
/// Spelled out apart from `src/auth.rs`, as [`super::auth`] does for `v1`.
pub fn user_auth_headers(
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    method: &str,
    path: &str,
) -> Result<Vec<(&'static str, String)>, getrandom::Error> {
    user_auth_headers_at(
        signing_key,
        user_id,
        key_id,
        method,
        path,
        chrono::Utc::now().to_rfc3339(),
    )
}

/// [`user_auth_headers`] at a caller-chosen timestamp, for the freshness tests.
pub fn user_auth_headers_at(
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    method: &str,
    path: &str,
    timestamp: String,
) -> Result<Vec<(&'static str, String)>, getrandom::Error> {
    let nonce = crate::auth::nonce()?;
    let signature = signing_key.sign(
        format!("v2\n{method}\n{path}\n{timestamp}\n{user_id}\n{key_id}\n{nonce}").as_bytes(),
    );
    Ok(vec![
        // The same header a client uses, widened to carry either prefix.
        ("X-Client-Id", user_id.to_string()),
        ("X-User-Key-Id", key_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(signature.to_bytes())),
        ("X-Nonce", nonce),
    ])
}

/// Auth headers signing `method path` with `key_id` alone, as the app's
/// key-only `GET /api/v1/users/me` does: no `X-Client-Id`, and an empty principal field.
pub fn user_key_only_headers(
    signing_key: &SigningKey,
    key_id: &UserKeyId,
    method: &str,
    path: &str,
) -> Result<Vec<(&'static str, String)>, getrandom::Error> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let nonce = crate::auth::nonce()?;
    let signature = signing_key
        .sign(format!("v2\n{method}\n{path}\n{timestamp}\n\n{key_id}\n{nonce}").as_bytes());
    Ok(vec![
        ("X-User-Key-Id", key_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(signature.to_bytes())),
        ("X-Nonce", nonce),
    ])
}

/// Create an active account directly in the store, the way the operator CLI
/// does, awaiting its first sign-in. `email` must be unique across the accounts
/// a test creates.
pub async fn create_user(state: &AppState, email: &str) -> anyhow::Result<User> {
    let user = User::try_new(ContactEmail::try_new(email)?, chrono::Utc::now())?;
    state
        .user_store
        .claim_user_email(user.contact_email.as_str(), &user.user_id)
        .await?;
    state.user_store.put_user(&user).await?;
    state
        .user_store
        .mark_user_awaiting_sign_in(&user.user_id)
        .await?;
    Ok(user)
}

/// Mint a single-use pre-auth key admitting a machine to `user_id`. Returns the
/// one-time token to present at registration.
pub async fn mint_user_preauth(state: &AppState, user_id: &UserId) -> anyhow::Result<String> {
    let minted = pipette_core::user_preauth::mint(
        UserPreauthParams {
            user_id: user_id.clone(),
            usage: UserPreauthUsage::SingleUse,
            expires_at: None,
            note: None,
        },
        chrono::Utc::now(),
    )?;
    state.user_store.put_user_preauth_key(&minted.key).await?;
    Ok(minted.token.expose().to_string())
}

/// Register `signing_key`'s public key against the account `token` names, over
/// HTTP. Returns the id the key was registered under.
pub async fn register_user_key(
    state: &AppState,
    token: &str,
    signing_key: &SigningKey,
    key_details: &str,
) -> anyhow::Result<UserKeyId> {
    let app = test_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/users/register")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json!({
                    "public_key": hex::encode(signing_key.verifying_key().as_bytes()),
                    "user_preauth_key": token,
                    "key_details": key_details,
                }))?))?,
        )
        .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    anyhow::ensure!(
        status == axum::http::StatusCode::CREATED,
        "user key registration failed with {status}: {body}"
    );
    Ok(UserKeyId::try_new(
        body.get("key_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing key_id in register response"))?,
    )?)
}

/// Ask for an account with no invite, the way `pipette-ctl auth register
/// --contact-email` does. Returns the raw response, because half the tests here
/// are about which refusal it is.
pub async fn request_user_account(
    state: &AppState,
    email: &str,
    signing_key: &SigningKey,
    key_details: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    super::request::unauthed_post(
        state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(signing_key.verifying_key().as_bytes()),
            "contact_email": email,
            "key_details": key_details,
        }),
    )
    .await
}

/// The contact address `names=true` gives `user_id`.
pub async fn user_address(state: &AppState, user_id: &UserId) -> anyhow::Result<String> {
    let user = state
        .user_store
        .get_user(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no account {user_id}"))?;
    Ok(user.contact_email.to_string())
}

/// The grants of an account that may revise its own campaign: it files plans,
/// cancels the plan a revision replaces, and reads the documents it revises at
/// the full tier.
pub const FULL_EDITOR: &[UserPermission] = &[
    UserPermission::SubmitPlans,
    UserPermission::CancelPlans,
    UserPermission::ReadClients,
];

/// An active account holding exactly `permissions`, with one key registered
/// over HTTP — the setup most user-route tests open with.
pub async fn registered_user(
    state: &AppState,
    permissions: &[UserPermission],
) -> anyhow::Result<(SigningKey, UserId, UserKeyId)> {
    // Unique per call: the address is claimed exclusively.
    let email = format!(
        "user-{}@example.test",
        hex::encode(uuid::Uuid::now_v7().as_bytes())
    );
    let user = create_user(state, &email).await?;
    // An empty set writes no object, as an account nobody granted holds none.
    if !permissions.is_empty() {
        grant_user(state, &user.user_id, permissions).await?;
    }
    let token = mint_user_preauth(state, &user.user_id).await?;
    let signing_key = crate::auth::signing_key()?;
    let key_id = register_user_key(state, &token, &signing_key, "test laptop").await?;
    Ok((signing_key, user.user_id, key_id))
}

/// An account that files plans, at one fleet-detail tier: `full` adds `read_clients`
/// to the plan grants, and the fleet-disclosure surfaces answer it in full.
pub async fn registered_user_with_detail(
    state: &AppState,
    full: bool,
) -> anyhow::Result<(SigningKey, UserId, UserKeyId)> {
    let mut permissions = vec![UserPermission::SubmitPlans, UserPermission::ReadPlans];
    if full {
        permissions.push(UserPermission::ReadClients);
    }
    registered_user(state, &permissions).await
}

/// Add `permission` to the plan grants `user_id` already holds, or leave the
/// set alone when it is `None`. The tier tests parameterize on that option.
pub async fn grant_plan_user_detail(
    state: &AppState,
    user_id: &UserId,
    permission: Option<UserPermission>,
) -> anyhow::Result<()> {
    let Some(permission) = permission else {
        return Ok(());
    };
    grant_user(
        state,
        user_id,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::CancelPlans,
            permission,
        ],
    )
    .await
}

/// Replace `user_id`'s stored permission set with exactly `permissions`.
pub async fn grant_user(
    state: &AppState,
    user_id: &UserId,
    permissions: &[UserPermission],
) -> anyhow::Result<()> {
    let set: UserPermissionSet = permissions.iter().copied().collect();
    state.user_store.put_user_permissions(user_id, &set).await?;
    Ok(())
}

/// Set `user_id`'s status, for the tests whose subject is a disabled account.
pub async fn set_user_status(
    state: &AppState,
    user_id: &UserId,
    status: UserStatus,
) -> anyhow::Result<()> {
    let mut user = state
        .user_store
        .get_user(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user {user_id} not found"))?;
    user.status = status;
    state.user_store.put_user(&user).await?;
    Ok(())
}

/// Send a POST signed as `user_id`, return the response. The user-side
/// counterpart of [`super::request::authed_post`].
pub async fn authed_user_post(
    state: &AppState,
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    uri: &str,
    body: &serde_json::Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = user_auth_headers(signing_key, user_id, key_id, "POST", uri)?;
    super::request::json_request(state, "POST", uri, &headers, body).await
}

/// The authority every RFC 9421 test request names, as its `Host`.
pub const TEST_AUTHORITY: &str = "mgmt.test";

/// RFC 9421 headers for a request signed by `signing_key`: `Host`, the two
/// signature fields and, with a body, `Content-Digest` and `Content-Length`.
pub fn rfc9421_headers(
    signing_key: &SigningKey,
    key_id: &UserKeyId,
    method: &str,
    uri: &str,
    body: Option<&[u8]>,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    rfc9421_headers_at(
        signing_key,
        key_id,
        method,
        uri,
        body,
        chrono::Utc::now().timestamp(),
    )
}

/// [`rfc9421_headers`] with `created` chosen by the caller.
pub fn rfc9421_headers_at(
    signing_key: &SigningKey,
    key_id: &UserKeyId,
    method: &str,
    uri: &str,
    body: Option<&[u8]>,
    created: i64,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    rfc9421_headers_for(
        TEST_AUTHORITY,
        signing_key,
        key_id,
        method,
        uri,
        body,
        created,
    )
}

/// [`rfc9421_headers_at`] signed for, and sent to, `authority`.
pub fn rfc9421_headers_for(
    authority: &str,
    signing_key: &SigningKey,
    key_id: &UserKeyId,
    method: &str,
    uri: &str,
    body: Option<&[u8]>,
    created: i64,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    let nonce = crate::auth::nonce()?;
    let signed = pipette_httpsig::sign(
        signing_key,
        key_id,
        &pipette_httpsig::Nonce::try_new(nonce)?,
        created,
        &pipette_httpsig::Outgoing {
            method,
            authority,
            path_and_query: uri,
            body,
        },
    )?;
    let mut headers = vec![
        ("Host", authority.to_string()),
        ("Signature-Input", signed.signature_input),
        ("Signature", signed.signature),
    ];
    if let (Some(digest), Some(body)) = (signed.content_digest, body) {
        headers.push(("Content-Digest", digest));
        headers.push(("Content-Length", body.len().to_string()));
    }
    Ok(headers)
}

/// Send a request signed with RFC 9421 by `signer`, with `body` as JSON.
pub async fn rfc9421_request(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
    body: Option<&serde_json::Value>,
) -> anyhow::Result<axum::http::Response<Body>> {
    let (key, _, key_id) = signer;
    let bytes = body.map(serde_json::to_vec).transpose()?;
    let headers = rfc9421_headers(key, key_id, method, uri, bytes.as_deref())?;
    match bytes {
        Some(bytes) => {
            super::request::raw_request(
                state,
                method,
                uri,
                &headers,
                Some("application/json"),
                bytes,
            )
            .await
        }
        None => super::request::request_with_headers(state, method, uri, &headers).await,
    }
}

/// Send a GET signed as `user_id`, return the response.
pub async fn authed_user_get(
    state: &AppState,
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = user_auth_headers(signing_key, user_id, key_id, "GET", uri)?;
    super::request::request_with_headers(state, "GET", uri, &headers).await
}

/// Send a DELETE signed as `user_id`, return the response.
pub async fn authed_user_delete(
    state: &AppState,
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = user_auth_headers(signing_key, user_id, key_id, "DELETE", uri)?;
    super::request::request_with_headers(state, "DELETE", uri, &headers).await
}

/// A signed-in account as the administration tests carry it around.
pub type UserSigner = (SigningKey, UserId, UserKeyId);

/// Send a signed request carrying a JSON body with a caller-chosen method, so
/// one helper covers `POST`, `PATCH` and `PUT`.
pub async fn signed_json(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
    body: &serde_json::Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let (key, user_id, key_id) = signer;
    let headers = user_auth_headers(key, user_id, key_id, method, uri)?;
    super::request::json_request(state, method, uri, &headers, body).await
}

/// Send a signed request with no body: the lifecycle verbs and the deletes.
pub async fn signed_empty(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let (key, user_id, key_id) = signer;
    let headers = user_auth_headers(key, user_id, key_id, method, uri)?;
    super::request::request_with_headers(state, method, uri, &headers).await
}

/// An organization written straight to the store, for a test whose subject is
/// what membership reaches rather than the route that creates one.
pub async fn seed_org(state: &AppState, slug: &str) -> anyhow::Result<Organization> {
    let org = Organization::new(
        OrgSlug::try_new(slug)?,
        NonEmptyTrimmedString::try_new(slug)?,
        chrono::Utc::now(),
    );
    state
        .org_store
        .claim_org_slug(org.slug.as_str(), &org.org_id)
        .await?;
    state.org_store.put_org(&org).await?;
    Ok(org)
}

/// Set `user_id`'s membership through the store, as `users set-org` would.
pub async fn set_user_org(
    state: &AppState,
    user_id: &UserId,
    org_id: Option<OrgId>,
) -> anyhow::Result<()> {
    state
        .user_store
        .get_user(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user {user_id} not found"))?;
    state.org_store.set_membership(user_id, org_id).await
}

/// Make `user_id` a service account on `org`, as `users set-type service` and
/// `users set-org` would.
pub async fn make_service_account(
    state: &AppState,
    user_id: &UserId,
    org_id: Option<OrgId>,
) -> anyhow::Result<()> {
    let mut user = state
        .user_store
        .get_user(user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user {user_id} not found"))?;
    user.account_type = UserAccountType::Service;
    state.user_store.put_user(&user).await?;
    set_user_org(state, user_id, org_id).await
}

/// Create an organization over HTTP as `signer` and return its minted id.
pub async fn create_org(
    state: &AppState,
    signer: &UserSigner,
    slug: &str,
) -> anyhow::Result<String> {
    let resp = signed_json(
        state,
        signer,
        "POST",
        "/api/v1/orgs",
        &serde_json::json!({ "slug": slug }),
    )
    .await?;
    anyhow::ensure!(
        resp.status() == axum::http::StatusCode::CREATED,
        "org create failed"
    );
    Ok(super::request::json_str(&super::request::body_json(resp).await?, "org_id")?.to_string())
}
