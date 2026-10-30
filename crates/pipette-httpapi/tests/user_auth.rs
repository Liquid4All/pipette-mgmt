//! Request authentication for a **user**: the `v2` signed payload, the key
//! registration it verifies against, and the claim that keeps one keypair to
//! one principal.

use axum::body::Body;
use axum::http::StatusCode;
use ed25519_dalek::SigningKey;
use rstest::rstest;
use serde_json::json;

use pipette_httpapi::state::AppState;
use pipette_plan_types::{UserPermission, UserStatus};
use pipette_web_testkit::{
    authed_user_delete, authed_user_get, authed_user_post, body_json, create_user, json_str,
    make_service_account, make_state, make_state_require_user_preauth,
    make_state_with_legacy_signatures, mint_user_preauth, register_user_key, registered_user,
    request_user_account, request_with_headers, seed_org, set_user_org, setup_benchmarks,
    unauthed_post, user_auth_headers, user_auth_headers_at,
};

// axum's `JsonRejection` text, which `ApiJson` passes through; an axum upgrade
// that rewords it is one edit here.
const AXUM_JSON_DATA_ERROR: &str = "Failed to deserialize the JSON body into the target type";
const AXUM_JSON_SYNTAX_ERROR: &str = "Failed to parse the request body as JSON";
const AXUM_JSON_CONTENT_TYPE_ERROR: &str = "Expected request with `Content-Type: application/json`";

async fn empty_state() -> anyhow::Result<(tempfile::TempDir, AppState)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    Ok((dir, state))
}

/// A tempdir, an `AppState` over it, and an account with one registered key.
async fn user_state() -> anyhow::Result<(
    tempfile::TempDir,
    AppState,
    SigningKey,
    pipette_plan_types::UserId,
    pipette_plan_types::UserKeyId,
)> {
    let (dir, state) = empty_state().await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    Ok((dir, state, key, user_id, key_id))
}

// ── the happy path ────────────────────────────────────────────

#[tokio::test]
async fn test_a_registered_key_authenticates() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("OwnUserView", &body)?;
    assert_eq!(body["user_id"], json!(user_id.as_str()));
    assert_eq!(body["status"], json!("active"));
    assert_eq!(body["keys"][0]["key_id"], json!(key_id.as_str()));
    assert_eq!(body["permissions"], json!(["read_plans"]));
    Ok(())
}

/// Needs no roster grant; an account in no organization carries neither field.
#[tokio::test]
async fn test_users_me_names_the_members_organization() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let body =
        body_json(authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?)
            .await?;
    crate::api_schema::assert_conforms("OwnUserView", &body)?;
    assert!(body.get("org_id").is_none(), "{body}");
    assert!(body.get("org_slug").is_none(), "{body}");

    let acme = seed_org(&state, "acme").await?;
    set_user_org(&state, &user_id, Some(acme.org_id.clone())).await?;
    let body =
        body_json(authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?)
            .await?;
    crate::api_schema::assert_conforms("OwnUserView", &body)?;
    assert_eq!(body["org_id"], json!(acme.org_id.as_str()));
    assert_eq!(body["org_slug"], json!("acme"));
    Ok(())
}

/// The resolved set, not the stored one, so a wide grant shows what it implies.
#[tokio::test]
async fn test_the_account_reads_its_resolved_permissions() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let stored = state
        .user_store
        .get_user_permissions(&user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no permission object"))?;
    assert_eq!(
        stored.names(),
        vec!["submit_plans"],
        "the store keeps the grant as given"
    );
    let user = state
        .user_store
        .get_user(&user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no record for the account"))?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("OwnUserView", &body)?;
    assert_eq!(body["permissions"], json!(["read_plans", "submit_plans"]));
    assert_eq!(body["contact_email"], json!(user.contact_email.as_str()));
    let created_at = chrono::DateTime::parse_from_rfc3339(json_str(&body, "created_at")?)?;
    assert_eq!(created_at, user.created_at);
    Ok(())
}

// ── forging the principal ─────────────────────────────────────

/// Fails at the lookup, not at a comparison: the key lives under exactly one
/// account, so the path holds nothing.
#[tokio::test]
async fn test_a_key_does_not_authenticate_another_account() -> anyhow::Result<()> {
    let (_dir, state, key, _user_id, key_id) = user_state().await?;
    let victim = create_user(&state, "victim@example.test").await?;

    // Signed correctly; only the key was never registered there.
    let headers = user_auth_headers(&key, &victim.user_id, &key_id, "GET", "/api/v1/users/me")?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Swapping `X-User-Key-Id` for another of the account's keys does not verify.
#[tokio::test]
async fn test_the_key_id_is_signed_not_merely_sent() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let second = pipette_web_testkit::signing_key()?;
    let headers = user_auth_headers(&key, &user_id, &key_id, "POST", "/api/v1/users/me/keys")?;
    let resp = pipette_web_testkit::json_request(
        &state,
        "POST",
        "/api/v1/users/me/keys",
        &headers,
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "second laptop",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let receipt = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserKeyRegistered", &receipt)?;
    let second_key_id = pipette_web_testkit::json_str(&receipt, "key_id")?.to_string();

    // Both keys are registered here, so only the payload tells them apart.
    let mut headers = user_auth_headers(&key, &user_id, &key_id, "GET", "/api/v1/users/me")?;
    if let Some(header) = headers.iter_mut().find(|h| h.0 == "X-User-Key-Id") {
        header.1 = second_key_id;
    }
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[rstest]
#[case::another_path("GET", "/somewhere-else")]
#[case::another_method("POST", "/api/v1/users/me")]
#[tokio::test]
async fn test_a_signature_binds_the_request(
    #[case] signed_method: &str,
    #[case] signed_path: &str,
) -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let headers = user_auth_headers(&key, &user_id, &key_id, signed_method, signed_path)?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[rstest]
#[case::another_query(
    "/api/v1/users/me?page=1",
    StatusCode::UNAUTHORIZED,
    "error",
    "invalid signature"
)]
#[case::the_same_query("/api/v1/users/me?page=2", StatusCode::OK, "status", "active")]
#[tokio::test]
async fn test_a_signature_binds_the_query_string(
    #[case] signed_uri: &str,
    #[case] expected: StatusCode,
    #[case] field: &str,
    #[case] value: &str,
) -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let headers = user_auth_headers(&key, &user_id, &key_id, "GET", signed_uri)?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me?page=2", &headers).await?;

    assert_eq!(resp.status(), expected);
    assert_eq!(json_str(&body_json(resp).await?, field)?, value);
    Ok(())
}

#[tokio::test]
async fn test_a_signature_is_spent_once() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let headers = user_auth_headers(&key, &user_id, &key_id, "GET", "/api/v1/users/me")?;
    let first = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;
    assert_eq!(first.status(), StatusCode::OK);

    let replay = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(replay).await?, "error")?,
        "signature already used"
    );
    Ok(())
}

#[tokio::test]
async fn test_a_stale_signature_is_refused() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let stale = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let headers = user_auth_headers_at(&key, &user_id, &key_id, "GET", "/api/v1/users/me", stale)?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "timestamp expired"
    );
    Ok(())
}

/// So the caller learns which credential to fix rather than a bare mismatch.
#[rstest]
#[case::no_key_id("X-User-Key-Id", None, "missing X-User-Key-Id header")]
#[case::no_timestamp("X-Timestamp", None, "missing X-Timestamp header")]
#[case::no_signature("X-Signature", None, "missing X-Signature header")]
#[case::no_nonce("X-Nonce", None, "missing X-Nonce header")]
#[case::empty_nonce("X-Nonce", Some(""), "missing X-Nonce header")]
#[case::key_id_without_its_prefix(
    "X-User-Key-Id",
    Some("user_abc"),
    "invalid X-User-Key-Id header: user key id must start with `userkey_`"
)]
#[case::unparsable_timestamp("X-Timestamp", Some("not-a-time"), "invalid timestamp format")]
#[case::non_hex_signature("X-Signature", Some("zz"), "invalid signature hex")]
// Ten bytes. The key is registered, so the request gets as far as decoding it.
#[case::short_signature(
    "X-Signature",
    Some("00000000000000000000"),
    "invalid signature length"
)]
#[tokio::test]
async fn test_a_missing_or_malformed_signing_header_is_refused_by_name(
    #[case] header: &str,
    #[case] replacement: Option<&str>,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    // `None` drops the header; `Some` overwrites its value.
    let headers: Vec<(&str, String)> =
        user_auth_headers(&key, &user_id, &key_id, "GET", "/api/v1/users/me")?
            .into_iter()
            .filter_map(|(name, value)| match (name == header, replacement) {
                (false, _) => Some((name, value)),
                (true, Some(replacement)) => Some((name, replacement.to_string())),
                (true, None) => None,
            })
            .collect();
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, expected_error);
    Ok(())
}

/// Refused by its id's prefix, before any key lookup: a client id never names
/// an account.
#[tokio::test]
async fn test_a_device_is_refused_on_a_user_route() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let (client_key, client_id) = pipette_web_testkit::register_and_approve(&state).await?;

    let resp = pipette_web_testkit::authed_get(&state, &client_key, &client_id, "/api/v1/users/me")
        .await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "invalid X-Client-Id header: user id must start with `user_`"
    );
    Ok(())
}

/// A `401`, not a `500`: registration checks hex and length only, so the
/// refusal falls to verification.
#[tokio::test]
async fn test_an_off_curve_user_key_is_the_callers_fault() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "offcurve@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode([0x02u8; 32]),
            "user_preauth_key": token,
            "key_details": "a key no laptop can sign for",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let key_id = json_str(&body_json(resp).await?, "key_id")?.to_string();

    let headers = vec![
        ("X-Client-Id", user.user_id.to_string()),
        ("X-User-Key-Id", key_id),
        ("X-Timestamp", chrono::Utc::now().to_rfc3339()),
        ("X-Signature", hex::encode([0u8; 64])),
        ("X-Nonce", "any".to_string()),
    ];
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "invalid public key"
    );
    Ok(())
}

/// The forgery below verifies any message under lenient verification, so this
/// fails the moment the user path stops verifying strictly.
#[tokio::test]
async fn test_a_small_order_user_key_cannot_be_signed_for() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "smallorder@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;
    // The identity point as a public key, and `R` = identity with `s` = 0 as
    // its signature.
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut forged = [0u8; 64];
    forged[0] = 1;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(identity),
            "user_preauth_key": token,
            "key_details": "a key anyone could sign for",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let key_id = json_str(&body_json(resp).await?, "key_id")?.to_string();

    let headers = vec![
        ("X-Client-Id", user.user_id.to_string()),
        ("X-User-Key-Id", key_id),
        ("X-Timestamp", chrono::Utc::now().to_rfc3339()),
        ("X-Signature", hex::encode(forged)),
        ("X-Nonce", "any".to_string()),
    ];
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "invalid signature"
    );
    Ok(())
}

/// Even where clients still get it: the flag is a per-client migration aid, and
/// a kind that never had it starts strict.
#[tokio::test]
async fn test_a_user_has_no_timestamp_only_fallback() -> anyhow::Result<()> {
    use ed25519_dalek::Signer;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let timestamp = chrono::Utc::now().to_rfc3339();
    let signature = key.sign(timestamp.as_bytes());
    let headers = vec![
        ("X-Client-Id", user_id.to_string()),
        ("X-User-Key-Id", key_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(signature.to_bytes())),
        ("X-Nonce", "any".to_string()),
    ];
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

// ── the account's own state ───────────────────────────────────

/// A `403`, not a `401`: it authenticated, and then was turned away.
#[tokio::test]
async fn test_a_disabled_account_is_refused() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    pipette_web_testkit::set_user_status(&state, &user_id, UserStatus::Disabled).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user is disabled"
    );
    Ok(())
}

/// The record, not the key, decides whether the account exists.
#[tokio::test]
async fn test_a_key_whose_account_is_gone_authenticates_nobody() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    state.user_store.delete_user(&user_id).await?;
    assert_eq!(
        state.user_store.list_user_keys(&user_id).await?.len(),
        1,
        "the registration must outlive the record for the request to reach the check"
    );

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user not found"
    );
    Ok(())
}

/// The signature is checked before the account. `None` deletes the record.
#[rstest]
#[case::disabled(Some(UserStatus::Disabled))]
#[case::gone(None)]
#[tokio::test]
async fn test_a_bad_signature_reveals_nothing_about_the_account(
    #[case] status: Option<UserStatus>,
) -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    match status {
        Some(status) => pipette_web_testkit::set_user_status(&state, &user_id, status).await?,
        None => state.user_store.delete_user(&user_id).await?,
    }

    // The account's own key, signing another request.
    let headers = user_auth_headers(&key, &user_id, &key_id, "GET", "/somewhere-else")?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "invalid signature"
    );
    Ok(())
}

#[tokio::test]
async fn test_an_added_key_is_receipted_and_signs() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    let second = pipette_web_testkit::signing_key()?;

    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "second laptop",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["user_id"], json!(user_id.as_str()));
    assert_eq!(body["status"], json!("active"));
    // `userkey_` and the hex sha256 of the public key, derived independently.
    let second_key_id = pipette_plan_types::UserKeyId::try_new(format!(
        "userkey_{}",
        pipette_web_testkit::sha256_hex(second.verifying_key().as_bytes())
    ))?;
    assert_eq!(body["key_id"], json!(second_key_id.as_str()));

    let resp = authed_user_get(
        &state,
        &second,
        &user_id,
        &second_key_id,
        "/api/v1/users/me",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let mut listed: Vec<&str> = body["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k["key_id"].as_str())
        .collect();
    listed.sort_unstable();
    let mut expected = vec![key_id.as_str(), second_key_id.as_str()];
    expected.sort_unstable();
    assert_eq!(listed, expected);
    Ok(())
}

/// A service account adds no key itself: its administrator mints the job a
/// pre-auth key, so a leaked key cannot register company for itself.
#[tokio::test]
async fn test_a_service_account_adds_no_key_itself() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    let org = seed_org(&state, "acme").await?;
    make_service_account(&state, &user_id, Some(org.org_id)).await?;
    let second = pipette_web_testkit::signing_key()?;

    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "second runner",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        json_str(&body_json(resp).await?, "error")?
            .contains("a service account adds no key itself")
    );
    assert_eq!(state.user_store.list_user_keys(&user_id).await?.len(), 1);
    Ok(())
}

/// The idempotent retry, not a conflict.
#[tokio::test]
async fn test_adding_a_key_the_account_holds_rewrites_it() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "key_details": "renamed laptop",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        json_str(&body_json(resp).await?, "key_id")?,
        key_id.as_str()
    );
    let keys = state.user_store.list_user_keys(&user_id).await?;
    assert_eq!(
        keys.iter()
            .map(|k| (k.key_id.as_str(), k.key_details.as_str()))
            .collect::<Vec<_>>(),
        vec![(key_id.as_str(), "renamed laptop")],
        "a retry must leave one registration, rewritten"
    );
    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// Including the key that signed the revocation: the compromised-laptop case.
#[tokio::test]
async fn test_a_revoked_key_stops_authenticating() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    let resp = authed_user_delete(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/users/me/keys/{key_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn test_revoking_one_key_leaves_the_other_signing() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    let second = pipette_web_testkit::signing_key()?;
    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "second laptop",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let second_key_id =
        pipette_plan_types::UserKeyId::try_new(json_str(&body_json(resp).await?, "key_id")?)?;

    let resp = authed_user_delete(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/users/me/keys/{second_key_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["keys"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["keys"][0]["key_id"], json!(key_id.as_str()));

    let resp = authed_user_get(
        &state,
        &second,
        &user_id,
        &second_key_id,
        "/api/v1/users/me",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, "key not found");
    Ok(())
}

/// Not a `204`, even when another account holds it: that key goes on signing
/// and its keypair stays claimed.
#[tokio::test]
async fn test_revoking_another_accounts_key_is_reported() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    let (other_key, other_id, other_key_id) =
        registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let resp = authed_user_delete(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/users/me/keys/{other_key_id}"),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "no such key on this account"
    );
    let resp = authed_user_get(
        &state,
        &other_key,
        &other_id,
        &other_key_id,
        "/api/v1/users/me",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // A released claim would let the caller take the keypair for itself.
    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(other_key.verifying_key().as_bytes()),
            "key_details": "a borrowed key",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "this keypair is already registered to another principal"
    );
    Ok(())
}

/// It reaches `GET /api/v1/users/me` and nothing else until it is admitted.
#[tokio::test]
async fn test_a_pending_account_cannot_revoke_its_key() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;
    let resp = request_user_account(&state, "pending@example.test", &key, "laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let user_id = pipette_plan_types::UserId::try_new(json_str(&body, "user_id")?)?;
    let key_id = pipette_plan_types::UserKeyId::try_new(json_str(&body, "key_id")?)?;

    let resp = authed_user_delete(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/users/me/keys/{key_id}"),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user is pending an operator's approval"
    );
    assert_eq!(
        state.user_store.list_user_keys(&user_id).await?.len(),
        1,
        "a refused revocation must leave the key registered"
    );
    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

// ── one keypair, one principal ────────────────────────────────

/// The partition bypass: a holder would collect `perform_work` on one principal
/// and `submit_plans` on the other, and pick per request.
#[tokio::test]
async fn test_a_users_keypair_cannot_also_register_as_a_client() -> anyhow::Result<()> {
    let (_dir, state, key, _user_id, _key_id) = user_state().await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "organization": "test-org",
            "client_details": "the same laptop",
            "contact_email": "t@example.test",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn test_a_clients_keypair_cannot_also_register_as_a_user() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;

    let (client_key, _client_id) = pipette_web_testkit::register_and_approve(&state).await?;
    let user = create_user(&state, "sharer@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(client_key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "the same device",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn test_two_accounts_cannot_share_a_keypair() -> anyhow::Result<()> {
    let (_dir, state, key, _user_id, _key_id) = user_state().await?;
    let other = create_user(&state, "other@example.test").await?;
    let token = mint_user_preauth(&state, &other.user_id).await?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "a borrowed key",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

// ── registration ──────────────────────────────────────────────

/// Reported `active`, which is how a caller tells it from the request path.
#[tokio::test]
async fn test_an_invited_registration_receipts_the_invited_account() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "invited@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;
    let key = pipette_web_testkit::signing_key()?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "work laptop",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await?,
        json!({
            "user_id": user.user_id.as_str(),
            "status": "active",
            "key_id": format!(
                "userkey_{}",
                pipette_web_testkit::sha256_hex(key.verifying_key().as_bytes())
            ),
        })
    );
    Ok(())
}

#[rstest]
#[case::unknown_key("user_preauth_deadbeef.wrongsecret")]
#[case::a_clients_token("client_preauth_deadbeef.secret")]
#[case::malformed("not-a-token")]
#[tokio::test]
async fn test_registration_requires_a_valid_preauth_key(#[case] token: &str) -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "laptop",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Bounded because its object is read on every authenticated request from the
/// account; one character past the ceiling registers nothing.
#[rstest]
#[case::at_the_ceiling(4096, StatusCode::CREATED)]
#[case::past_the_ceiling(4097, StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_key_details_is_bounded(
    #[case] chars: usize,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "bounded@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;
    let key = pipette_web_testkit::signing_key()?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "d".repeat(chars),
        }),
    )
    .await?;

    assert_eq!(resp.status(), expected);
    assert_eq!(
        state.user_store.list_user_keys(&user.user_id).await?.len(),
        usize::from(expected == StatusCode::CREATED),
        "a refused description must register nothing"
    );
    Ok(())
}

/// It claims no address: the same request, well formed, is then a `201`, not
/// a replay's `200`.
#[rstest]
#[case::public_key_not_hex(
    "public_key",
    Some(json!("zz")),
    "public_key: public_key must be valid hex"
)]
#[case::public_key_too_short(
    "public_key",
    Some(json!(hex::encode([0u8; 16]))),
    "public_key: public_key must be 32 bytes (64 hex chars), got 16"
)]
// The path prefix is what ties this generic message to the field.
#[case::blank_key_details(
    "key_details",
    Some(json!("   ")),
    "key_details: must be a non-empty string"
)]
#[case::invalid_contact_email(
    "contact_email",
    Some(json!("not-an-address")),
    "contact_email: contact_email is not a valid email address"
)]
// The two `missing field` reasons are serde's wording, not the project's.
#[case::no_public_key("public_key", None, "missing field `public_key`")]
#[case::no_key_details("key_details", None, "missing field `key_details`")]
#[tokio::test]
async fn test_a_register_field_that_does_not_read_is_a_400(
    #[case] field: &str,
    #[case] value: Option<serde_json::Value>,
    #[case] expected_reason: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;
    let mut body = json!({
        "public_key": hex::encode(key.verifying_key().as_bytes()),
        "contact_email": "asker@example.test",
        "key_details": "work laptop",
    });
    // `None` drops the field; `Some` overwrites its value.
    match value {
        Some(value) => body[field] = value,
        None => {
            body.as_object_mut()
                .and_then(|fields| fields.remove(field))
                .ok_or_else(|| anyhow::anyhow!("the body carries {field}"))?;
        }
    }

    let resp = unauthed_post(&state, "/api/v1/users/register", &body).await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.starts_with(AXUM_JSON_DATA_ERROR), "{error}");
    assert!(error.contains(expected_reason), "{error}");
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("asker@example.test")
            .await?,
        None,
        "a refused body must claim no address"
    );
    let resp = request_user_account(&state, "asker@example.test", &key, "work laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

/// In the error envelope, not axum's own `400` or `415`, and it claims no
/// address.
#[rstest]
#[case::truncated(Some("application/json"), true, AXUM_JSON_SYNTAX_ERROR)]
#[case::text_plain(Some("text/plain"), false, AXUM_JSON_CONTENT_TYPE_ERROR)]
#[case::no_content_type(None, false, AXUM_JSON_CONTENT_TYPE_ERROR)]
#[tokio::test]
async fn test_a_register_body_not_read_as_json_is_a_400(
    #[case] content_type: Option<&str>,
    #[case] truncate: bool,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;
    let whole = serde_json::to_string(&json!({
        "public_key": hex::encode(key.verifying_key().as_bytes()),
        "contact_email": "asker@example.test",
        "key_details": "work laptop",
    }))?;
    let body = if truncate {
        whole
            .strip_suffix('}')
            .ok_or_else(|| anyhow::anyhow!("a serialized object ends in a brace"))?
            .to_string()
    } else {
        whole
    };

    let resp = pipette_web_testkit::raw_request(
        &state,
        "POST",
        "/api/v1/users/register",
        &[],
        content_type,
        body,
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.starts_with(expected_error), "{error}");
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("asker@example.test")
            .await?,
        None,
        "a refused body must claim no address"
    );
    let resp = request_user_account(&state, "asker@example.test", &key, "work laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn test_a_single_use_preauth_key_admits_one_machine() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "once@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;

    let first = pipette_web_testkit::signing_key()?;
    register_user_key(&state, &token, &first, "first laptop").await?;

    let second = pipette_web_testkit::signing_key()?;
    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "second laptop",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Deleting an account leaves its pre-auth keys behind.
#[tokio::test]
async fn test_a_preauth_key_outliving_its_account_registers_nothing() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "deleted@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;
    pipette_pipeline::user_admin::delete(
        &*state.user_store,
        &*state.key_index_store,
        &*state.org_store,
        &user.user_id,
    )
    .await?;
    assert!(
        state
            .user_store
            .list_user_preauth_keys()
            .await?
            .iter()
            .any(|k| k.user_id == user.user_id),
        "the pre-auth key must outlive the account for the request to reach the account check"
    );
    let key = pipette_web_testkit::signing_key()?;

    let resp = unauthed_post(
        &state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "laptop",
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "invalid pre-auth key"
    );
    assert!(
        state
            .user_store
            .list_user_keys(&user.user_id)
            .await?
            .is_empty(),
        "a refused registration must land no key"
    );
    assert!(
        state.user_store.get_user(&user.user_id).await?.is_none(),
        "a refused registration must not bring the account back"
    );
    // A claim left behind would refuse this with a 409.
    let other = create_user(&state, "elsewhere@example.test").await?;
    let token = mint_user_preauth(&state, &other.user_id).await?;
    register_user_key(&state, &token, &key, "laptop").await?;
    Ok(())
}

/// Registration writes no permissions: an account keeps what it was granted,
/// and one granted nothing still holds no object.
#[rstest]
#[case::granted(&[UserPermission::CancelPlans], Some(vec!["cancel_plans"]))]
#[case::never_granted(&[], None)]
#[tokio::test]
async fn test_registering_with_a_preauth_key_leaves_the_permissions_alone(
    #[case] granted: &[UserPermission],
    #[case] expected: Option<Vec<&str>>,
) -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;

    let (_, user_id, _) = registered_user(&state, granted).await?;

    let stored = state.user_store.get_user_permissions(&user_id).await?;
    assert_eq!(stored.as_ref().map(|set| set.names()), expected);
    Ok(())
}

/// An unauthenticated request reaches no user route but the registration one.
#[rstest]
#[case("GET", "/api/v1/users/me")]
#[case("POST", "/api/v1/users/me/keys")]
#[case("DELETE", "/api/v1/users/me/keys/userkey_abc")]
#[tokio::test]
async fn test_the_user_routes_need_a_signature(
    #[case] method: &str,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let (_dir, state, _key, _user_id, _key_id) = user_state().await?;

    let resp: axum::http::Response<Body> = request_with_headers(&state, method, path, &[]).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

// ── asking for an account ─────────────────────────────────────

/// The account exists at once and may do nothing until an operator admits it.
#[tokio::test]
async fn test_a_machine_can_request_an_account() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;

    let resp = request_user_account(&state, "asker@example.test", &key, "work laptop").await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserKeyRegistered", &body)?;
    assert_eq!(body["status"], "pending");
    let user_id = pipette_plan_types::UserId::try_new(
        body["user_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no user_id"))?,
    )?;
    let key_id = pipette_plan_types::UserKeyId::try_new(
        body["key_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no key_id"))?,
    )?;

    // The address is folded: the record *is* the key it was claimed at.
    let user = state
        .user_store
        .get_user(&user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no record for the requested account"))?;
    assert_eq!(user.status, UserStatus::Pending);
    assert_eq!(user.contact_email.as_str(), "asker@example.test");
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("asker@example.test")
            .await?,
        Some(user_id.clone())
    );

    // Its own record is how it watches for the decision.
    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["status"], "pending");

    let second = pipette_web_testkit::signing_key()?;
    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "a second laptop",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The same key, re-registering nothing, now reaches the routes it was
    // refused.
    pipette_web_testkit::set_user_status(&state, &user_id, UserStatus::Active).await?;
    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "a second laptop",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

/// Both, or neither, is a `400` before any key is spent or record written.
#[rstest]
#[case::neither(false, false)]
#[case::both(true, true)]
#[tokio::test]
async fn test_an_invite_and_an_address_are_exclusive(
    #[case] with_invite: bool,
    #[case] with_address: bool,
) -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let user = create_user(&state, "invited@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;
    let key = pipette_web_testkit::signing_key()?;

    let mut body = json!({
        "public_key": hex::encode(key.verifying_key().as_bytes()),
        "key_details": "work laptop",
    });
    if with_invite {
        body["user_preauth_key"] = json!(token);
    }
    if with_address {
        body["contact_email"] = json!("asker@example.test");
    }
    let resp = unauthed_post(&state, "/api/v1/users/register", &body).await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        state
            .user_store
            .list_user_keys(&user.user_id)
            .await?
            .is_empty(),
        "a refused body must register nothing"
    );
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("asker@example.test")
            .await?,
        None,
        "a refused body must claim no address"
    );
    // The single-use invite was not spent: it still admits the machine.
    register_user_key(&state, &token, &key, "work laptop").await?;
    Ok(())
}

/// Admitting a second machine would have an operator approve a stranger's key
/// on someone else's account.
#[tokio::test]
async fn test_an_address_admits_one_requested_account() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;

    let first = pipette_web_testkit::signing_key()?;
    let resp = request_user_account(&state, "shared@example.test", &first, "the laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let holder = pipette_plan_types::UserId::try_new(
        body_json(resp).await?["user_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no user_id"))?,
    )?;

    let second = pipette_web_testkit::signing_key()?;
    let resp = request_user_account(&state, "shared@example.test", &second, "someone else").await?;

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        state.user_store.list_user_keys(&holder).await?.len(),
        1,
        "the refused request must not land a key on the account holding the address"
    );
    Ok(())
}

/// The caller retrying a `201` it never saw reads its account, not a `409`.
#[tokio::test]
async fn test_asking_again_with_the_same_key_replays_the_account() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;

    let first =
        body_json(request_user_account(&state, "again@example.test", &key, "laptop").await?)
            .await?;
    let again = request_user_account(&state, "again@example.test", &key, "laptop, again").await?;

    assert_eq!(again.status(), StatusCode::OK);
    let again = body_json(again).await?;
    assert_eq!(again["user_id"], first["user_id"]);
    assert_eq!(again["key_id"], first["key_id"]);
    assert_eq!(again["status"], "pending");
    Ok(())
}

/// A retry in another case replays; a stranger in another case meets the `409`.
#[tokio::test]
async fn test_a_requested_address_is_folded_to_lower_case() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;

    let resp = request_user_account(&state, "Asker@Example.test", &key, "work laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let first = body_json(resp).await?;
    let user_id = pipette_plan_types::UserId::try_new(json_str(&first, "user_id")?)?;
    let user = state
        .user_store
        .get_user(&user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no record for the requested account"))?;
    assert_eq!(user.contact_email.as_str(), "asker@example.test");
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("asker@example.test")
            .await?,
        Some(user_id)
    );

    let again = request_user_account(&state, "ASKER@example.TEST", &key, "work laptop").await?;
    assert_eq!(again.status(), StatusCode::OK);
    let again = body_json(again).await?;
    assert_eq!(again["user_id"], first["user_id"]);
    assert_eq!(again["key_id"], first["key_id"]);

    let stranger = pipette_web_testkit::signing_key()?;
    let resp =
        request_user_account(&state, "asker@EXAMPLE.test", &stranger, "someone else").await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "an account already holds that address"
    );
    Ok(())
}

/// So the caller's next attempt, with a fresh keypair, is one round trip rather
/// than an operator's problem.
#[tokio::test]
async fn test_a_refused_keypair_leaves_the_address_free() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let (client_key, _client_id) = pipette_web_testkit::register_and_approve(&state).await?;

    let resp =
        request_user_account(&state, "retry@example.test", &client_key, "that device").await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let fresh = pipette_web_testkit::signing_key()?;
    let resp = request_user_account(&state, "retry@example.test", &fresh, "a new keypair").await?;

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the address claim must not outlive the registration it was made for"
    );
    Ok(())
}

/// An operator's to resolve: it looks like a request still in flight, so
/// adopting it would take an address out from under one.
#[tokio::test]
async fn test_a_stranded_claim_is_not_adopted_by_a_request() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    // What a create or a request that died between its two writes leaves.
    let stranded =
        pipette_plan_types::UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
    state
        .user_store
        .claim_user_email("stranded@example.test", &stranded)
        .await?;

    let key = pipette_web_testkit::signing_key()?;
    let resp = request_user_account(&state, "stranded@example.test", &key, "work laptop").await?;

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("stranded@example.test")
            .await?,
        Some(stranded),
        "the refused request must leave the claim where it found it"
    );
    Ok(())
}

/// The invited path goes on working, as with `require_client_preauth_key`.
#[tokio::test]
async fn test_requesting_an_account_can_be_closed_off() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_require_user_preauth(dir.path()).await?;
    let key = pipette_web_testkit::signing_key()?;

    let resp = request_user_account(&state, "closed@example.test", &key, "work laptop").await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("closed@example.test")
            .await?,
        None,
        "a refused request must claim nothing"
    );

    let user = create_user(&state, "invited@example.test").await?;
    let token = mint_user_preauth(&state, &user.user_id).await?;
    register_user_key(&state, &token, &key, "work laptop").await?;
    Ok(())
}

// ── the claim's other half: freeing it ────────────────────────

/// A claim left behind would retire the keypair for good: nothing else releases
/// one.
#[tokio::test]
async fn test_revoking_a_key_frees_its_keypair() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;

    authed_user_delete(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/users/me/keys/{key_id}"),
    )
    .await?;

    let token = mint_user_preauth(&state, &user_id).await?;
    let reused = register_user_key(&state, &token, &key, "same laptop, again").await?;

    assert_eq!(reused, key_id, "a key id is the keypair's digest");
    let resp = authed_user_get(&state, &key, &user_id, &reused, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

// ── a key-only signature ──────────────────────────────────────

/// Pending included: how a holder of nothing but the key learns its account.
#[tokio::test]
async fn test_users_me_names_the_account_a_key_only_signature_holds() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let key = pipette_web_testkit::signing_key()?;
    let resp = request_user_account(&state, "pending@example.test", &key, "laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let receipt = body_json(resp).await?;
    let key_id = pipette_plan_types::UserKeyId::try_new(json_str(&receipt, "key_id")?)?;

    let headers =
        pipette_web_testkit::user_key_only_headers(&key, &key_id, "GET", "/api/v1/users/me")?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("OwnUserView", &body)?;
    assert_eq!(body["user_id"], receipt["user_id"]);
    assert_eq!(body["status"], json!("pending"));
    Ok(())
}

#[tokio::test]
async fn test_a_key_only_signature_reaches_no_other_route() -> anyhow::Result<()> {
    let (_dir, state, key, _user_id, key_id) = user_state().await?;
    let path = format!("/api/v1/users/me/keys/{key_id}");

    let headers = pipette_web_testkit::user_key_only_headers(&key, &key_id, "DELETE", &path)?;
    let resp = request_with_headers(&state, "DELETE", &path, &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "missing X-Client-Id header"
    );
    Ok(())
}

// ── expiry ────────────────────────────────────────────────────

/// One already past, or further out than `MAX_KEY_LIFETIME`, is refused.
#[tokio::test]
async fn test_a_key_added_with_an_expiry_lists_it() -> anyhow::Result<()> {
    let (_dir, state, key, user_id, key_id) = user_state().await?;
    let browser = pipette_web_testkit::signing_key()?;
    let public_key = hex::encode(browser.verifying_key().as_bytes());
    // At the cap, from a caller whose clock runs a few minutes ahead.
    let expires_at = chrono::Utc::now()
        + pipette_httpapi::api_v1::users::own::MAX_KEY_LIFETIME
        + chrono::TimeDelta::minutes(4);

    let past = json!({
        "public_key": public_key,
        "key_details": "a browser",
        "expires_at": chrono::Utc::now() - chrono::TimeDelta::seconds(1),
    });
    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &past,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "expires_at has already passed"
    );

    let too_far = json!({
        "public_key": public_key,
        "key_details": "a browser",
        "expires_at": chrono::Utc::now()
            + pipette_httpapi::api_v1::users::own::MAX_KEY_LIFETIME
            + chrono::TimeDelta::hours(1),
    });
    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &too_far,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "expires_at is more than 30 days away"
    );

    let body = json!({
        "public_key": public_key,
        "key_details": "a browser",
        "expires_at": expires_at,
    });
    crate::api_schema::assert_conforms("AddUserKeyRequest", &body)?;
    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let me = body_json(authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?)
        .await?;
    crate::api_schema::assert_conforms("OwnUserView", &me)?;
    let listed = me["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find(|k| k["key_details"] == "a browser"))
        .ok_or_else(|| anyhow::anyhow!("the browser key is listed: {me}"))?;
    let listed_at = chrono::DateTime::parse_from_rfc3339(json_str(listed, "expires_at")?)?;
    assert_eq!(listed_at, expires_at);
    let own = me["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find(|k| k["key_id"] == json!(key_id.as_str())))
        .ok_or_else(|| anyhow::anyhow!("the machine key is listed: {me}"))?;
    assert!(
        own.get("expires_at").is_none(),
        "a machine key never expires"
    );
    Ok(())
}

#[tokio::test]
async fn test_an_expired_key_signs_nothing() -> anyhow::Result<()> {
    let (_dir, state, _key, user_id, _key_id) = user_state().await?;
    let browser = pipette_web_testkit::signing_key()?;
    let public_key =
        pipette_plan_types::PublicKeyHex::try_new(hex::encode(browser.verifying_key().as_bytes()))?;
    let lapsed = pipette_core::user::UserKey {
        expires_at: Some(chrono::Utc::now() - chrono::TimeDelta::seconds(1)),
        ..pipette_core::user::UserKey::new(
            public_key,
            pipette_plan_types::NonEmptyTrimmedString::try_new("a browser")?,
            chrono::Utc::now() - chrono::TimeDelta::days(30),
        )
    };
    state.user_store.put_user_key(&user_id, &lapsed).await?;

    let resp = authed_user_get(
        &state,
        &browser,
        &user_id,
        &lapsed.key_id,
        "/api/v1/users/me",
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, "key expired");
    Ok(())
}
