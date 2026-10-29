//! User requests signed with RFC 9421: the key names the account, the signature
//! covers the method, authority, path, query and body, and `v2` keeps working
//! beside it.

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::json;

use pipette_core::user_permission::UserPermissionGroup;
use pipette_plan_types::{UserPermission, UserStatus};
use pipette_web_testkit::*;

async fn empty_state() -> anyhow::Result<(tempfile::TempDir, pipette_httpapi::state::AppState)> {
    state_with_authorities(&[TEST_AUTHORITY]).await
}

async fn state_with_authorities(
    authorities: &[&str],
) -> anyhow::Result<(tempfile::TempDir, pipette_httpapi::state::AppState)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.authorities = authorities.iter().map(|a| a.to_string()).collect();
    let state = make_state_from_config(config).await?;
    Ok((dir, state))
}

async fn error_of(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    Ok(body_json(resp)
        .await?
        .pointer("/error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// The headers with `name` replaced by `value`.
fn with(
    headers: &[(&'static str, String)],
    name: &str,
    value: &str,
) -> Vec<(&'static str, String)> {
    headers
        .iter()
        .map(|(n, v)| {
            (
                *n,
                if n.eq_ignore_ascii_case(name) {
                    value.to_string()
                } else {
                    v.clone()
                },
            )
        })
        .collect()
}

/// The key's registration names the account.
#[tokio::test]
async fn a_key_signs_without_naming_its_account() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let signer = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let resp = rfc9421_request(&state, &signer, "GET", "/api/v1/users/me", None).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        json_str(&body_json(resp).await?, "user_id")?,
        signer.1.as_str()
    );

    let resp = rfc9421_request(&state, &signer, "GET", "/api/v1/plans?limit=1", None).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// A write is held to the body its `Content-Digest` names, and runs when it is
/// that body.
#[tokio::test]
async fn a_write_is_held_to_its_body() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let admin = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let (key, _, key_id) = &admin;

    let signed = serde_json::to_vec(&json!({ "slug": "acme" }))?;
    let sent = serde_json::to_vec(&json!({ "slug": "evil" }))?;
    let headers = rfc9421_headers(key, key_id, "POST", "/api/v1/orgs", Some(&signed))?;
    let headers = with(&headers, "Content-Length", &sent.len().to_string());
    let resp = raw_request(
        &state,
        "POST",
        "/api/v1/orgs",
        &headers,
        Some("application/json"),
        sent,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        error_of(resp)
            .await?
            .contains("not the signed Content-Digest")
    );
    assert!(
        state.org_store.list_orgs().await?.is_empty(),
        "nothing was written"
    );

    let resp = rfc9421_request(
        &state,
        &admin,
        "POST",
        "/api/v1/orgs",
        Some(&json!({ "slug": "acme" })),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

/// A body the signature covered no digest for is refused: declared, before
/// any key is read; undeclared, as the empty body the signature vouched for.
#[tokio::test]
async fn a_body_without_a_signed_digest_is_refused() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let (key, _, key_id) =
        registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let body = serde_json::to_vec(&json!({ "slug": "acme" }))?;
    let headers = rfc9421_headers(&key, &key_id, "POST", "/api/v1/orgs", None)?;

    let mut declared = headers.clone();
    declared.push(("Content-Length", body.len().to_string()));
    let resp = raw_request(
        &state,
        "POST",
        "/api/v1/orgs",
        &declared,
        Some("application/json"),
        body.clone(),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(error_of(resp).await?.contains("must cover content-digest"));

    let resp = raw_request(
        &state,
        "POST",
        "/api/v1/orgs",
        &headers,
        Some("application/json"),
        body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        error_of(resp)
            .await?
            .contains("not the signed Content-Digest")
    );
    assert!(state.org_store.list_orgs().await?.is_empty());
    Ok(())
}

/// Each covered part binds: a different method, authority, path or query, or
/// a reused signature, is refused.
#[tokio::test]
async fn each_covered_part_binds() -> anyhow::Result<()> {
    let (_dir, state) = state_with_authorities(&[TEST_AUTHORITY, "elsewhere.test"]).await?;
    let (key, _, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let headers = rfc9421_headers(&key, &key_id, "GET", "/api/v1/campaigns?limit=1", None)?;

    for (method, uri, headers) in [
        ("GET", "/api/v1/campaigns?limit=2", headers.clone()),
        ("GET", "/api/v1/plans?limit=1", headers.clone()),
        ("POST", "/api/v1/campaigns?limit=1", headers.clone()),
        (
            "GET",
            "/api/v1/campaigns?limit=1",
            with(&headers, "Host", "elsewhere.test"),
        ),
    ] {
        let resp = request_with_headers(&state, method, uri, &headers).await?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(error_of(resp).await?, "invalid signature", "{method} {uri}");
    }

    let resp = request_with_headers(&state, "GET", "/api/v1/campaigns?limit=1", &headers).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = request_with_headers(&state, "GET", "/api/v1/campaigns?limit=1", &headers).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error_of(resp).await?, "signature already used");
    Ok(())
}

/// RFC 9421 beside a `v2` header is refused, so no header names a principal the
/// signature did not.
#[tokio::test]
async fn a_request_signs_one_way() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let mut headers = rfc9421_headers(&key, &key_id, "GET", "/api/v1/users/me", None)?;
    headers.push(("X-Client-Id", user_id.to_string()));
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(error_of(resp).await?.contains("signs one way"));

    // `v2` still authenticates on its own.
    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// Only an account's key names an account.
#[tokio::test]
async fn a_device_key_names_no_account() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (device_key, device) = register_and_approve(&state).await?;
    let key_id = pipette_plan_types::UserKeyId::try_new(format!(
        "userkey_{}",
        device.as_str().trim_start_matches("ev1_")
    ))?;
    let headers = rfc9421_headers(&device_key, &key_id, "GET", "/api/v1/users/me", None)?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error_of(resp).await?, "key not found");
    Ok(())
}

/// Standing is read as for `v2`: a disabled account reaches nothing, and a
/// pending one reaches only its own record.
#[tokio::test]
async fn standing_is_read_as_for_v2() -> anyhow::Result<()> {
    let (_dir, state) = empty_state().await?;
    let signer = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    set_user_status(&state, &signer.1, UserStatus::Pending).await?;
    let resp = rfc9421_request(&state, &signer, "GET", "/api/v1/users/me", None).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = rfc9421_request(&state, &signer, "GET", "/api/v1/plans", None).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    set_user_status(&state, &signer.1, UserStatus::Disabled).await?;
    let resp = rfc9421_request(&state, &signer, "GET", "/api/v1/users/me", None).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_of(resp).await?, "user is disabled");
    Ok(())
}

/// The gates that dispatch on the principal's kind read an RFC 9421 request
/// as an account's: filing a plan, the catalog, and a vault upload.
#[tokio::test]
async fn the_kind_dispatching_gates_admit_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let filer = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let (key, user_id, key_id) = &filer;

    let uri = own_campaign_plans_uri(&state, user_id).await?;
    let plan = ONE_CELL_PLAN.as_bytes();
    let mut headers = rfc9421_headers(key, key_id, "POST", &uri, Some(plan))?;
    headers.retain(|(n, _)| *n != "Content-Length");
    headers.push(("Content-Length", plan.len().to_string()));
    let resp = raw_request(
        &state,
        "POST",
        &uri,
        &headers,
        Some("application/toml"),
        plan.to_vec(),
    )
    .await?;
    assert!(
        resp.status().is_success(),
        "{}: {}",
        resp.status(),
        error_of(resp).await?
    );

    let resp = rfc9421_request(&state, &filer, "GET", "/benchmarks", None).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let file = b"weights".as_slice();
    let digest = sha256_hex(file);
    let uri = format!("/api/v1/vault/blobs/{user_id}/{digest}");
    let headers = rfc9421_headers(key, key_id, "PUT", &uri, Some(file))?;
    let resp = raw_request(
        &state,
        "PUT",
        &uri,
        &headers,
        Some("application/octet-stream"),
        file.to_vec(),
    )
    .await?;
    assert!(
        resp.status().is_success(),
        "{}: {}",
        resp.status(),
        error_of(resp).await?
    );

    let other = format!("/api/v1/vault/blobs/{user_id}/{}", sha256_hex(b"other"));
    let headers = rfc9421_headers(key, key_id, "PUT", &other, Some(file))?;
    let resp = raw_request(
        &state,
        "PUT",
        &other,
        &headers,
        Some("application/octet-stream"),
        file.to_vec(),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(error_of(resp).await?.contains("is not the path's digest"));
    Ok(())
}

/// The forgery verifies any message under lenient verification, so this fails
/// the moment the RFC 9421 path stops verifying strictly.
#[tokio::test]
async fn a_small_order_key_cannot_be_signed_for() -> anyhow::Result<()> {
    use base64::Engine;

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

    let created = chrono::Utc::now().timestamp();
    let headers = vec![
        ("Host", TEST_AUTHORITY.to_string()),
        (
            "Signature-Input",
            format!(
                r#"sig=("@method" "@authority" "@path" "@query");created={created};nonce="any";keyid="{key_id}";alg="ed25519";tag="pipette""#
            ),
        ),
        (
            "Signature",
            format!(
                "sig=:{}:",
                base64::engine::general_purpose::STANDARD.encode(forged)
            ),
        ),
    ];
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error_of(resp).await?, "invalid signature");
    Ok(())
}

/// A request signed for another deployment holding the same key cannot be replayed here.
#[rstest]
#[case::ours(&[TEST_AUTHORITY], TEST_AUTHORITY, StatusCode::OK)]
#[case::any_case(&["Mgmt.Test"], "MGMT.test", StatusCode::OK)]
#[case::another_deployment(&[TEST_AUTHORITY], "mgmt.other", StatusCode::UNAUTHORIZED)]
#[case::another_port(&[TEST_AUTHORITY], "mgmt.test:8443", StatusCode::UNAUTHORIZED)]
#[case::none_configured(&[], TEST_AUTHORITY, StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn a_signature_for_another_authority_is_refused(
    #[case] ours: &[&str],
    #[case] signed_for: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state) = state_with_authorities(ours).await?;
    let (key, _, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let headers = rfc9421_headers_for(
        signed_for,
        &key,
        &key_id,
        "GET",
        "/api/v1/users/me",
        None,
        chrono::Utc::now().timestamp(),
    )?;
    let resp = request_with_headers(&state, "GET", "/api/v1/users/me", &headers).await?;
    assert_eq!(resp.status(), expected);
    if expected == StatusCode::UNAUTHORIZED {
        assert!(error_of(resp).await?.contains("does not answer to"));
    }
    Ok(())
}

/// With `accept_v2_signatures` off, `v2`, which covers no authority, is refused
/// whether or not it names the account, and RFC 9421 still signs.
#[rstest]
#[case::naming_the_account(false)]
#[case::key_only(true)]
#[tokio::test]
async fn v2_is_refused_once_it_is_switched_off(#[case] key_only: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.accept_v2_signatures = false;
    let state = make_state_from_config(config).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let me = "/api/v1/users/me";

    let v2 = if key_only {
        user_key_only_headers(&key, &key_id, "GET", me)?
    } else {
        user_auth_headers(&key, &user_id, &key_id, "GET", me)?
    };
    let resp = request_with_headers(&state, "GET", me, &v2).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(error_of(resp).await?.contains("sign with RFC 9421"));

    let headers = rfc9421_headers(&key, &key_id, "GET", me, None)?;
    let resp = request_with_headers(&state, "GET", me, &headers).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}
