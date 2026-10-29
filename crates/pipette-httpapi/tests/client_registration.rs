//! `POST /clients/register`: the one route that mints an identity, and the only
//! one an unauthenticated caller reaches that writes. A registration refused
//! before the key is consumed leaves it unspent.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

use std::collections::BTreeSet;

use anyhow::Context;
use axum::http::StatusCode;
use futures::{StreamExt, TryStreamExt};
use rstest::rstest;
use serde_json::json;

use pipette_core::client_permission::StoredClientPermission;
use pipette_core::client_preauth::ClientPreauthParams;
use pipette_core::config::AutoApproveConfig;
use pipette_core::key_index::{KeyHolderPrincipalId, digest_of};
use pipette_core::types::{DeriveFromKey, Mint};
use pipette_httpapi::registration_throttle::{
    KEYLESS_REGISTRATIONS_PER_CLIENT, KEYLESS_REGISTRATIONS_PER_WINDOW,
};
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    ClientId, ClientOwner, ClientPermission, ClientPreauthUsage, ExpiresAt, NonEmptyTrimmedString,
    PublicKeyHex, Tag, UserId, UserPermission,
};
use pipette_web_testkit::{
    authed_get, body_json, create_user, grant, json_request, json_str, make_state,
    make_state_from_config, make_state_require_client_preauth, make_state_with_auto_approve,
    raw_request, register_and_approve, register_pending, registered_user, request_user_account,
    run_qm, seed_avail, seed_org, set_user_org, setup_benchmarks, signed_empty, signed_json,
    test_config, unauthed_post,
};

async fn mint_stored_key(state: &AppState, params: ClientPreauthParams) -> anyhow::Result<String> {
    let minted = pipette_core::client_preauth::mint(params, chrono::Utc::now())?;
    state
        .client_store
        .put_client_preauth_key(&minted.key)
        .await?;
    Ok(minted.token.expose().to_string())
}

fn key_params(usage: ClientPreauthUsage) -> ClientPreauthParams {
    ClientPreauthParams {
        usage,
        expires_at: None,
        default_tags: BTreeSet::new(),
        default_organization: None,
        default_owner: None,
        default_permissions: None,
        note: None,
    }
}

fn fresh_pubkey() -> anyhow::Result<String> {
    let sk = pipette_web_testkit::signing_key()?;
    Ok(hex::encode(sk.verifying_key().as_bytes()))
}

fn registered_id(body: &serde_json::Value) -> anyhow::Result<ClientId> {
    let id = body
        .get("client_id")
        .and_then(serde_json::Value::as_str)
        .context("a registration response carries a client_id")?;
    Ok(ClientId::try_new(id)?)
}

#[tokio::test]
async fn test_register_with_client_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let client_id = body["client_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing client_id"))?;
    assert!(client_id.starts_with("ev1_"));
    assert_eq!(body["status"], "pending");
    assert!(body.get("private_key").is_none());
    Ok(())
}

/// Register with `generate_key` and return the private key, checking it signs
/// as the returned `client_id`.
async fn register_generated(state: &AppState) -> anyhow::Result<[u8; 32]> {
    let resp = unauthed_post(
        state,
        "/clients/register",
        &json!({
            "generate_key": true,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let sk: [u8; 32] = hex::decode(
        body.get("private_key")
            .and_then(|v| v.as_str())
            .context("private_key")?,
    )?
    .try_into()
    .map_err(|_| anyhow::anyhow!("private_key is not 32 bytes"))?;
    let pk = PublicKeyHex::try_new(hex::encode(
        ed25519_dalek::SigningKey::from_bytes(&sk)
            .verifying_key()
            .as_bytes(),
    ))?;
    assert_eq!(body.get("client_id"), Some(&json!(ClientId::derive(&pk))));
    Ok(sk)
}

/// Each registration draws a fresh key.
#[tokio::test]
async fn test_register_with_generated_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    assert_ne!(
        register_generated(&state).await?,
        register_generated(&state).await?,
        "two registrations got the same private key"
    );
    Ok(())
}

/// A returned key's confidentiality rests on the deployment, so it is logged; a
/// client-supplied one is not, as announcing every registration names nothing.
#[rstest]
#[case::server_generated(true, true)]
#[case::client_supplied(false, false)]
#[test]
fn a_returned_private_key_is_announced_in_the_log(
    #[case] generate_key: bool,
    #[case] expect_logged: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;

    let (status, logs) = pipette_web_testkit::capture_logs(|rt| -> anyhow::Result<StatusCode> {
        rt.block_on(async {
            let state = make_state(dir.path()).await?;
            let mut body = json!({
                "organization": "TestOrg",
                "client_details": "test client",
                "contact_email": "test@test.com"
            });
            if generate_key {
                body["generate_key"] = json!(true);
            } else {
                body["public_key"] = json!(fresh_pubkey()?);
            }
            let resp = unauthed_post(&state, "/clients/register", &body).await?;
            Ok(resp.status())
        })
    })?;

    assert_eq!(status?, StatusCode::CREATED);
    // Every registration logs this, so an absent warning is not an empty capture.
    assert!(logs.contains("client registered"), "{logs}");
    assert_eq!(
        logs.contains("returned a server-generated private key"),
        expect_logged,
        "log said the wrong thing for a registration with generate_key={generate_key}:\n{logs}"
    );
    Ok(())
}

#[tokio::test]
async fn test_register_duplicate_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let body = json!({
        "public_key": pk_hex,
        "organization": "TestOrg",
        "client_details": "test",
        "contact_email": "t@t.com"
    });

    let first = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await?;

    // 200 with the same id and status, not 409.
    let second = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = body_json(second).await?;
    assert_eq!(second_body["client_id"], first_body["client_id"]);
    assert_eq!(second_body["status"], first_body["status"]);
    Ok(())
}

/// The route is unauthenticated, so anything a repeat refreshed would let
/// whoever holds a device's public key rewrite its record and matcher input.
#[tokio::test]
async fn test_register_repeat_with_different_fields_leaves_the_record_alone() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    // The repeat's address matches this rule, so re-evaluating it would approve.
    let state =
        make_state_with_auto_approve(dir.path(), vec![], vec!["second.test".to_string()]).await?;

    let public_key = fresh_pubkey()?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": public_key,
            "organization": "first-org",
            "client_details": "first details",
            "contact_email": "lab@first.example",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let first = body_json(resp).await?;
    assert_eq!(first["status"], "pending");
    let id = ClientId::try_new(json_str(&first, "client_id")?)?;
    let before = serde_json::to_value(
        state
            .client_store
            .get_client(&id)
            .await?
            .context("the first registration stores a record")?,
    )?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": public_key,
            "organization": "second-org",
            "client_details": "second details",
            "contact_email": "ops@second.test",
            "device_form_factor": "laptop",
            "device_ram_bytes": 16_000_000_000u64,
            "capabilities": ["runtime:llama_cpp"],
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let repeat = body_json(resp).await?;
    assert_eq!(
        repeat,
        json!({ "client_id": id.as_str(), "status": "pending" }),
        "a repeat returns the stored id and status, and no private key"
    );

    let after = serde_json::to_value(
        state
            .client_store
            .get_client(&id)
            .await?
            .context("the repeat keeps the record")?,
    )?;
    assert_eq!(after, before, "a repeat must not rewrite the record");
    assert_eq!(after["organization"], "first-org");
    assert!(
        !state.todo_store.has_pending_reindex(&id).await?,
        "a repeat must not flag a reindex"
    );
    Ok(())
}

#[tokio::test]
async fn test_register_rejects_both_public_key_and_generate_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "generate_key": true,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_register_trims_string_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": format!("  {pk_hex}\n"),
            "organization": "  TestOrg  ",
            "client_details": "\ttest client\n",
            "contact_email": " test@test.com "
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .context("the response body carries a string client_id")?,
    )?;

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("client should be stored")?;
    assert_eq!(stored.public_key.as_str(), pk_hex);
    assert_eq!(stored.organization.as_str(), "TestOrg");
    assert_eq!(stored.client_details.as_str(), "test client");
    assert_eq!(stored.contact_email.as_str(), "test@test.com");
    Ok(())
}

#[tokio::test]
async fn test_register_auto_approves_matching_email() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state =
        make_state_with_auto_approve(dir.path(), vec!["alice@example.com".to_string()], vec![])
            .await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // Mixed case should still match (case-insensitive equality).
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "Alice@Example.COM"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "approved");

    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .context("the response body carries a string client_id")?,
    )?;
    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("client should be stored")?;
    assert_eq!(stored.status, pipette_core::client::ClientStatus::Approved);
    Ok(())
}

#[tokio::test]
async fn test_register_auto_approves_matching_domain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state =
        make_state_with_auto_approve(dir.path(), vec![], vec!["example.org".to_string()]).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "someone@example.org"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "approved");
    Ok(())
}

#[tokio::test]
async fn test_register_non_matching_email_stays_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_auto_approve(
        dir.path(),
        vec!["alice@example.com".to_string()],
        vec!["example.org".to_string()],
    )
    .await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "nobody@example.net"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "pending");
    Ok(())
}

#[tokio::test]
async fn test_register_rejects_neither_public_key_nor_generate_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com"
        }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_register_with_device_profile_persists_and_flags_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com",
            "device_form_factor": "laptop",
            "device_ram_bytes": 36_000_000_000u64,
            "device_gpu_model": "M3 Pro GPU",
            "device_gpu_vram_bytes": 18_000_000_000u64
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;

    // The profile is persisted as flat `device_*` keys.
    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client not stored"))?;
    let stored = serde_json::to_value(&stored)?;
    assert_eq!(stored["device_form_factor"], "laptop");
    assert_eq!(stored["device_ram_bytes"], 36_000_000_000u64);
    assert_eq!(stored["device_gpu_model"], "M3 Pro GPU");

    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.iter().any(|(flagged, _)| flagged == &client_id));
    Ok(())
}

#[tokio::test]
async fn test_register_without_device_profile_does_not_flag_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com"
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_register_with_capabilities_persists_and_flags_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // Capabilities alone make the client matchable, so they flag a reindex.
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "TestOrg",
            "client_details": "test client",
            "contact_email": "test@test.com",
            "capabilities": ["runtime:llama_cpp", "runtime:mlx"]
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client not stored"))?;
    assert_eq!(
        stored.capabilities,
        std::collections::BTreeSet::from([
            "runtime:llama_cpp".to_string(),
            "runtime:mlx".to_string()
        ])
    );

    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.iter().any(|(flagged, _)| flagged == &client_id));
    Ok(())
}

#[rstest]
#[case::form_factor(json!({"device_form_factor": "spaceship"}), "must be one of")]
#[case::form_factor_empty(json!({"device_form_factor": ""}), "must be one of")]
#[case::gpu_vram(json!({"device_gpu_vram_bytes": 18_000_000_000u64}), "device_gpu_vram_bytes requires device_gpu_model")]
#[case::npu_vram(json!({"device_npu_vram_bytes": 8_000_000_000u64}), "device_npu_vram_bytes requires device_npu_model")]
#[case::os_version(json!({"device_os_version": "15.3"}), "device_os_version requires device_os_name")]
#[case::reserved_capability(json!({"capabilities": ["os:ios"]}), "reserved namespace")]
#[case::reserved_arch(json!({"capabilities": ["arch:x86_64"]}), "reserved namespace")]
#[case::tag_capability(json!({"capabilities": ["tag:lab"]}), "names a tag")]
#[case::empty_capability(json!({"capabilities": [" "]}), "must not be empty")]
// Non-canonical spellings fail before the reserved check, so none smuggles a
// reserved flag past it or reports an unmatchable mixed-case flag.
#[case::capability_uppercase_namespace(json!({"capabilities": ["OS:ios"]}), "lowercase")]
#[case::capability_leading_space(json!({"capabilities": [" os:ios"]}), "lowercase")]
#[case::capability_mixedcase_runtime(json!({"capabilities": ["runtime:Llama_CPP"]}), "lowercase")]
#[tokio::test]
async fn test_register_invalid_device_profile_returns_400(
    #[case] device_fields: serde_json::Value,
    #[case] want: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let mut body = json!({
        "public_key": pk_hex,
        "organization": "TestOrg",
        "client_details": "test client",
        "contact_email": "test@test.com",
    });
    body.as_object_mut()
        .zip(device_fields.as_object())
        .map(|(base, extra)| base.extend(extra.clone()))
        .ok_or_else(|| anyhow::anyhow!("expected json objects"))?;

    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains(want)),
        "error should contain {want:?}"
    );
    Ok(())
}

#[tokio::test]
async fn test_register_malformed_body_returns_400_json_envelope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // A newtype refusal at extraction is a `400` in the error envelope, not
    // axum's default `422` plain text.
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "",
            "client_details": "test client",
            "contact_email": "test@test.com"
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("non-empty string")),
        "expected the newtype validation message in the error envelope, got: {body:?}"
    );
    Ok(())
}

/// An absent identity field is a `400` naming it, and no client. A serde default
/// on any of them would register a device nobody can contact or place.
#[rstest]
#[case::organization("organization")]
#[case::client_details("client_details")]
#[case::contact_email("contact_email")]
#[tokio::test]
async fn test_register_missing_an_identity_field_is_refused(
    #[case] field: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let public_key = fresh_pubkey()?;
    let mut body = json!({
        "public_key": public_key,
        "organization": "o",
        "client_details": "d",
        "contact_email": "lab@example.test",
    });
    body.as_object_mut()
        .and_then(|fields| fields.remove(field))
        .ok_or_else(|| anyhow::anyhow!("the body carries {field}"))?;

    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    let want = format!("missing field `{field}`");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains(&want)),
        "expected {want:?}: {body}"
    );
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?,
        "a refused body must not create a client"
    );
    Ok(())
}

/// A body that is not JSON is a `400` in the error envelope, not axum's plain
/// text, so a client parses one error shape.
#[tokio::test]
async fn test_register_refuses_a_body_that_does_not_parse() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let public_key = fresh_pubkey()?;
    let body = serde_json::to_string(&json!({
        "public_key": public_key,
        "organization": "o",
        "client_details": "d",
        "contact_email": "lab@example.test",
    }))?;
    let truncated = body
        .strip_suffix('}')
        .ok_or_else(|| anyhow::anyhow!("a serialized object ends in a brace"))?
        .to_string();

    let resp = raw_request(
        &state,
        "POST",
        "/clients/register",
        &[],
        Some("application/json"),
        truncated,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // The message is axum's wording passed through, so only its presence is pinned.
    let body = body_json(resp).await?;
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{body}"
    );
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?,
        "a refused body must not create a client"
    );
    Ok(())
}

/// A valid body sent without a JSON `Content-Type` is a `400` in the envelope,
/// not axum's `415`: registration maps every extractor refusal but size to `400`.
#[rstest]
#[case::text_plain(Some("text/plain"))]
#[case::none(None)]
#[tokio::test]
async fn test_register_refuses_a_body_not_sent_as_json(
    #[case] content_type: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let public_key = fresh_pubkey()?;
    let body = serde_json::to_string(&json!({
        "public_key": public_key,
        "organization": "o",
        "client_details": "d",
        "contact_email": "lab@example.test",
    }))?;

    let resp = raw_request(&state, "POST", "/clients/register", &[], content_type, body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{body}"
    );
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?,
        "a refused body must not create a client"
    );
    Ok(())
}

#[tokio::test]
async fn test_client_preauth_key_auto_approves_and_seeds_tags_org() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut p = key_params(ClientPreauthUsage::SingleUse);
    p.default_tags = BTreeSet::from([Tag::try_new("team-mobile")?]);
    p.default_organization = Some(NonEmptyTrimmedString::try_new("seeded-org")?);
    let token = mint_stored_key(&state, p).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "client-org",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "approved");

    let id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;
    // Seeded tag is applied, and the key's org overrides the client-supplied one.
    assert_eq!(
        state
            .tag_store
            .list_client_ids_by_tag(&Tag::try_new("team-mobile")?)
            .await?,
        vec![id.clone()]
    );
    // A job can require the tag, so it alone flags the client for indexing.
    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.iter().any(|(flagged, _)| flagged == &id));
    let client = state
        .client_store
        .get_client(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client missing"))?;
    assert_eq!(client.organization.as_str(), "seeded-org");
    Ok(())
}

/// How a tagged pool registers: a job requiring the key's tag is offered to the
/// keyed client from its first index, and not to one registered without it.
#[tokio::test]
async fn test_a_seeded_tag_routes_work_to_the_new_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    // Indexed before either client exists, so only the reindex a registration
    // flags can offer it.
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"requires": ["tag:lab"]}),
    )?;
    run_qm(&state).await?;

    let mut p = key_params(ClientPreauthUsage::SingleUse);
    p.default_tags = BTreeSet::from([Tag::try_new("lab")?]);
    // Only a worker is indexed, so the key also grants `perform_work`.
    p.default_permissions = Some(BTreeSet::from([ClientPermission::PerformWork]));
    let token = mint_stored_key(&state, p).await?;
    // A reported flag, so the registration with no key flags itself too.
    let mut keyed = json!({
        "public_key": fresh_pubkey()?,
        "organization": "client-org",
        "client_details": "d",
        "contact_email": "a@b.com",
        "capabilities": ["runtime:llama_cpp"],
        "client_preauth_key": token,
    });
    let resp = unauthed_post(&state, "/clients/register", &keyed).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let seeded = registered_id(&body_json(resp).await?)?;

    keyed["public_key"] = json!(fresh_pubkey()?);
    keyed
        .as_object_mut()
        .context("a registration body")?
        .remove("client_preauth_key");
    let resp = unauthed_post(&state, "/clients/register", &keyed).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plain = registered_id(&body_json(resp).await?)?;

    run_qm(&state).await?;

    let offered: Vec<_> = state
        .todo_store
        .list_eligible_for_client(&seeded)
        .await?
        .into_iter()
        .map(|(job_id, _)| job_id)
        .collect();
    assert_eq!(offered, vec![job]);
    assert!(
        state
            .todo_store
            .list_eligible_for_client(&plain)
            .await?
            .is_empty()
    );
    Ok(())
}

enum OwnerKind {
    Org,
    User,
}

async fn seed_owner(state: &AppState, kind: OwnerKind) -> anyhow::Result<ClientOwner> {
    Ok(match kind {
        OwnerKind::Org => ClientOwner::Org {
            org_id: seed_org(state, "acme").await?.org_id,
        },
        OwnerKind::User => ClientOwner::User {
            user_id: create_user(state, "owner@example.test").await?.user_id,
        },
    })
}

/// A key naming an owner puts the device there at registration, for either
/// owner kind: minting the key is the operator vouching for the device.
#[rstest]
#[case::org(OwnerKind::Org)]
#[case::user(OwnerKind::User)]
#[tokio::test]
async fn test_client_preauth_key_seeds_the_owner(#[case] kind: OwnerKind) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let owner = seed_owner(&state, kind).await?;
    let mut p = key_params(ClientPreauthUsage::SingleUse);
    p.default_owner = Some(owner.clone());
    let token = mint_stored_key(&state, p).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "client-org",
            "client_details": "d",
            "contact_email": "lab@example.test",
            "client_preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;
    let client = state
        .client_store
        .get_client(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client missing"))?;
    assert_eq!(state.org_store.get_ownership(&id).await?, Some(owner));
    assert_eq!(
        client.organization.as_str(),
        "client-org",
        "the owner is not the free-text label"
    );
    Ok(())
}

/// A registration with no owner clears one left under its id, and any tags. The
/// id derives from the key, so a device deleted and registered again comes back
/// under it, and would otherwise inherit what a failed teardown left there.
#[tokio::test]
async fn test_a_registration_clears_a_leftover_owner_and_tags() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let pk = fresh_pubkey()?;
    let id = ClientId::derive(&PublicKeyHex::try_new(pk.clone())?);
    let leftover = seed_owner(&state, OwnerKind::Org).await?;
    state.org_store.set_ownership(&id, Some(leftover)).await?;
    let lab = Tag::try_new("lab")?;
    state.tag_store.add_client_tag(&id, &lab).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk,
            "organization": "o",
            "client_details": "d",
            "contact_email": "lab@example.test",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(state.org_store.get_ownership(&id).await?, None);
    assert!(state.tag_store.get_client_tags(&id).await?.is_empty());
    assert!(
        state
            .tag_store
            .list_client_ids_by_tag(&lab)
            .await?
            .is_empty()
    );
    Ok(())
}

enum Removal {
    Delete,
    Reject,
}

/// Back to `pending` under the same id with only the default grant: replaying
/// the old record would hand a removed device its approval and grants.
#[rstest]
#[case::delete(Removal::Delete)]
#[case::reject(Removal::Reject)]
#[tokio::test]
async fn test_a_removed_device_registers_again_as_new(
    #[case] removal: Removal,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;

    // Granted past the default either way, so a grant that survived would show.
    let (key, client_id, method, uri) = match removal {
        Removal::Delete => {
            let (key, id) = register_and_approve(&state).await?;
            let uri = format!("/api/v1/clients/{id}");
            (key, id, "DELETE", uri)
        }
        Removal::Reject => {
            let (key, id) = register_pending(&state).await?;
            grant(
                &state,
                &id,
                &[
                    ClientPermission::PerformWork,
                    ClientPermission::SubmitResults,
                ],
            )
            .await?;
            let uri = format!("/api/v1/clients/{id}/reject");
            (key, id, "POST", uri)
        }
    };
    let resp = signed_empty(&state, &admin, method, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{method} {uri}");

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "organization": "o",
            "client_details": "d",
            "contact_email": "lab@example.test",
        }),
    )
    .await?;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "a removed device is new, not a repeat"
    );
    let body = body_json(resp).await?;
    assert_eq!(body["client_id"], client_id.as_str());
    assert_eq!(body["status"], "pending");

    let resp = authed_get(&state, &key, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let me = body_json(resp).await?;
    assert_eq!(me["status"], "pending");
    assert_eq!(me["permissions"], json!(["submit_results"]));
    Ok(())
}

/// A `tags` field in the body tags nothing. Tags are operator-assigned, and a
/// device that tagged itself could steer which plans target it.
#[tokio::test]
async fn test_register_cannot_set_tags() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let key = pipette_web_testkit::signing_key()?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "organization": "o",
            "client_details": "d",
            "contact_email": "lab@example.test",
            "tags": ["sneaky"],
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let id = ClientId::try_new(json_str(&body, "client_id")?)?;

    assert!(
        state.tag_store.get_client_tags(&id).await?.is_empty(),
        "the body's tags must not reach the client's markers"
    );
    assert!(
        state
            .tag_store
            .list_client_ids_by_tag(&Tag::try_new("sneaky")?)
            .await?
            .is_empty(),
        "the body's tags must not reach the reverse index"
    );
    let resp = authed_get(&state, &key, &id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["tags"],
        json!([]),
        "a client cannot set its own tags"
    );
    Ok(())
}

/// Returns the public key too, so a refusal can be checked for a stray record.
async fn register_fresh(
    state: &AppState,
    token: Option<&str>,
) -> anyhow::Result<(StatusCode, serde_json::Value, String)> {
    let pk = fresh_pubkey()?;
    let mut body = json!({
        "public_key": pk,
        "organization": "o",
        "client_details": "d",
        "contact_email": "lab@example.test",
    });
    if let (Some(token), Some(fields)) = (token, body.as_object_mut()) {
        fields.insert("client_preauth_key".into(), json!(token));
    }
    let resp = unauthed_post(state, "/clients/register", &body).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?, pk))
}

/// Registering unowned would leave the device for any org admin to take. The
/// check runs after the consume, so a retry reads a single-use key as unknown.
#[rstest]
#[tokio::test]
async fn test_client_preauth_key_naming_a_deleted_owner_is_refused(
    #[values(OwnerKind::Org, OwnerKind::User)] kind: OwnerKind,
    #[values(ClientPreauthUsage::SingleUse, ClientPreauthUsage::MultiUse)]
    usage: ClientPreauthUsage,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let owner = seed_owner(&state, kind).await?;
    let mut p = key_params(usage);
    p.default_owner = Some(owner.clone());
    let token = mint_stored_key(&state, p).await?;
    match &owner {
        ClientOwner::Org { org_id } => state.org_store.delete_org(org_id).await?,
        ClientOwner::User { user_id } => state.user_store.delete_user(user_id).await?,
    }

    let retry = match usage {
        ClientPreauthUsage::SingleUse => "invalid pre-auth key",
        ClientPreauthUsage::MultiUse => "no longer exists",
    };
    for expected in ["no longer exists", retry] {
        let (status, body, pk) = register_fresh(&state, Some(&token)).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains(expected)),
            "expected {expected:?}: {body}"
        );
        assert!(
            !state
                .client_store
                .has_public_key(&PublicKeyHex::try_new(pk)?)
                .await?,
            "no client should be created on refusal"
        );
    }
    Ok(())
}

/// A device a key put in an organization belongs to it from registration, so
/// another organization's admin cannot take it by either route, nor detach it.
/// An ordinary registration arrives unowned, and only a server admin takes that.
#[tokio::test]
async fn test_a_key_owned_device_is_out_of_another_organizations_reach() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let acme = seed_org(&state, "acme").await?;
    let rival = seed_org(&state, "rival").await?;
    let rival_admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    set_user_org(&state, &rival_admin.1, Some(rival.org_id.clone())).await?;

    let acme_owner = ClientOwner::Org {
        org_id: acme.org_id.clone(),
    };
    let mut p = key_params(ClientPreauthUsage::SingleUse);
    p.default_owner = Some(acme_owner.clone());
    let token = mint_stored_key(&state, p).await?;
    let (status, body, _) = register_fresh(&state, Some(&token)).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let owned = body["client_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing client_id"))?
        .to_string();

    let rival_id = rival.org_id.as_str();
    for (uri, request) in [
        (
            format!("/api/v1/orgs/{rival_id}/clients"),
            json!({ "client_ids": [owned] }),
        ),
        (
            format!("/api/v1/clients/{owned}/owner"),
            json!({ "owner": { "type": "org", "org_id": rival_id } }),
        ),
        (
            format!("/api/v1/clients/{owned}/owner"),
            json!({ "owner": null }),
        ),
    ] {
        let resp = signed_json(&state, &rival_admin, "PUT", &uri, &request).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "PUT {uri} {request}");
    }
    let stored = state
        .org_store
        .get_ownership(&ClientId::try_new(owned.as_str())?)
        .await?;
    assert_eq!(stored, Some(acme_owner));

    let (_, body, _) = register_fresh(&state, None).await?;
    let unowned = body["client_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing client_id"))?;
    let resp = signed_json(
        &state,
        &rival_admin,
        "PUT",
        &format!("/api/v1/orgs/{rival_id}/clients"),
        &json!({ "client_ids": [unowned] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// No template falls back to the default; an empty template grants nothing.
#[rstest]
#[case::no_key(None, &["submit_results"])]
#[case::no_template(Some(None), &["submit_results"])]
#[case::template(
    Some(Some(&[ClientPermission::PerformWork, ClientPermission::SubmitResults][..])),
    &["perform_work", "submit_results"]
)]
#[case::empty_template(Some(Some(&[][..])), &[])]
#[tokio::test]
async fn test_registration_seeds_the_permission_set(
    #[case] template: Option<Option<&[ClientPermission]>>,
    #[case] expected: &[&str],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut body = json!({
        "public_key": fresh_pubkey()?,
        "organization": "client-org",
        "client_details": "d",
        "contact_email": "a@b.com",
    });
    if let Some(permissions) = template {
        let mut p = key_params(ClientPreauthUsage::SingleUse);
        p.default_permissions = permissions.map(|set| set.iter().copied().collect());
        body["client_preauth_key"] = json!(mint_stored_key(&state, p).await?);
    }

    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let response = body_json(resp).await?;
    let id = ClientId::try_new(
        response["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;

    // Read from the object, not the legacy default it would fall back to.
    let stored = state
        .client_store
        .get_client_permissions(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registration must write a permissions object"))?;
    let names: Vec<&str> = stored.iter().map(|p| p.wire_name()).collect();
    assert_eq!(names, expected);
    Ok(())
}

/// An unknown name and a user's name both seed verbatim: the CLI is the closed
/// gate, a stored template is open. Failing would `500` a valid registration;
/// dropping a name would rewrite a key an operator minted.
#[tokio::test]
async fn test_registration_seeds_around_a_template_name_it_cannot_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Widened as a newer binary, or one before the partition, would write it.
    let mut params = key_params(ClientPreauthUsage::SingleUse);
    params.default_permissions = Some(BTreeSet::from([ClientPermission::PerformWork]));
    let mut minted = pipette_core::client_preauth::mint(params, chrono::Utc::now())?;
    minted
        .key
        .default_permissions
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("template must be present"))?
        .extend([
            // Through `classify`, so the fixture carries the variant a reader
            // would have given the name rather than one this test asserts.
            StoredClientPermission::classify(pipette_plan_types::PermissionName::try_new(
                "operate_centrifuge",
            )?),
            StoredClientPermission::classify(pipette_plan_types::PermissionName::try_new(
                "submit_plans",
            )?),
        ]);
    state
        .client_store
        .put_client_preauth_key(&minted.key)
        .await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "client-org",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": minted.token.expose(),
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let response = body_json(resp).await?;
    let id = ClientId::try_new(
        response["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;
    let stored = state
        .client_store
        .get_client_permissions(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registration must write a permissions object"))?;

    // Both names are carried onto the client, and neither grants anything.
    let names: Vec<&str> = stored
        .iter()
        .map(StoredClientPermission::wire_name)
        .collect();
    assert_eq!(
        names,
        ["operate_centrifuge", "perform_work", "submit_plans"]
    );
    assert!(stored.holds(ClientPermission::PerformWork));
    assert!(!stored.holds(ClientPermission::SubmitResults));
    Ok(())
}

/// A record without its object would resolve to the frozen legacy default and
/// hold less than the operator granted. A regular file in place of the store's
/// directory fails the real write.
#[tokio::test]
async fn test_a_failed_permissions_write_fails_the_registration() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    std::fs::write(dir.path().join("client-permissions"), b"not a directory")?;

    let public_key = fresh_pubkey()?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": public_key,
            "organization": "client-org",
            "client_details": "d",
            "contact_email": "a@b.com",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // The record comes after the object, so nothing blocks a later retry.
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?,
        "a failed permissions write must not leave a client record"
    );
    Ok(())
}

/// A job can require a seeded tag, so a client registered without it would
/// never be offered its key's work. A file in place of the tag's reverse
/// directory fails the write after its forward marker lands.
#[tokio::test]
async fn test_a_failed_seeded_tag_write_fails_the_registration() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let mut p = key_params(ClientPreauthUsage::SingleUse);
    p.default_tags = BTreeSet::from([Tag::try_new("lab")?]);
    let token = mint_stored_key(&state, p).await?;
    let by_tag = dir.path().join("tags-index").join("by-tag");
    std::fs::create_dir_all(&by_tag)?;
    std::fs::write(by_tag.join("lab"), b"not a directory")?;

    let public_key = fresh_pubkey()?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": public_key,
            "organization": "client-org",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?,
        "a failed seeded-tag write must not leave a client record"
    );
    Ok(())
}

/// Not a silent drop, which would leave the client believing the grant took
/// effect. An explicit `null` still carries the key, so it is refused too.
#[rstest]
#[case::names(json!(["perform_work"]))]
#[case::empty(json!([]))]
#[case::null(serde_json::Value::Null)]
#[tokio::test]
async fn test_register_rejects_client_supplied_permissions(
    #[case] permissions: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let public_key = fresh_pubkey()?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": public_key,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "permissions": permissions,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert_eq!(body["error"], "permissions are assigned by an operator");

    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?
    );
    Ok(())
}

/// The refusal lands before a pre-auth key is consumed, so a client that
/// retries without the field still holds a usable key.
#[tokio::test]
async fn test_a_rejected_permissions_key_does_not_spend_a_client_preauth_key() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": token,
            "permissions": ["perform_work"],
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

/// A keypair a user holds is a `409` before the key is consumed, so the refusal
/// does not burn a single-use key the device can then present with its own.
#[tokio::test]
async fn test_a_keypair_conflict_does_not_spend_a_client_preauth_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (user_key, _, _) = registered_user(&state, &[]).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    let public_key = hex::encode(user_key.verifying_key().as_bytes());
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": public_key,
            "organization": "o",
            "client_details": "d",
            "contact_email": "lab@example.test",
            "client_preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await?,
        json!({ "error": "this keypair is already registered to another principal" })
    );
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(public_key)?)
            .await?,
        "the conflict must not create a client"
    );
    assert_eq!(
        state.client_store.list_client_preauth_keys().await?.len(),
        1,
        "the conflict must not spend the key"
    );

    let (status, body, _) = register_fresh(&state, Some(&token)).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "approved");
    Ok(())
}

/// A store fault strands the keypair claim as a crash would, and the retry with
/// the same keypair meets it as its own: a `201` under the same id, not a `409`.
#[tokio::test]
async fn test_a_retry_after_a_store_fault_meets_its_own_claim() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let blocker = dir.path().join("client-permissions");

    let public_key = PublicKeyHex::try_new(fresh_pubkey()?)?;
    let client_id = ClientId::derive(&public_key);
    let body = json!({
        "public_key": public_key.as_str(),
        "organization": "o",
        "client_details": "d",
        "contact_email": "lab@example.test",
    });
    std::fs::write(&blocker, b"not a directory")?;

    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await?,
        json!({ "error": "internal server error" })
    );
    assert!(
        !state.client_store.has_public_key(&public_key).await?,
        "a failed registration must not leave a client record"
    );
    assert_eq!(
        state
            .key_index_store
            .holder_of_public_key(&digest_of(&public_key))
            .await?,
        Some(KeyHolderPrincipalId::Client(client_id.clone())),
        "the store fault leaves the claim behind"
    );

    std::fs::remove_file(&blocker)?;
    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "a claim this id holds is not a conflict"
    );
    assert_eq!(
        body_json(resp).await?,
        json!({ "client_id": client_id.as_str(), "status": "pending" })
    );
    assert!(state.client_store.has_public_key(&public_key).await?);
    Ok(())
}

/// An un-updated client's key registers `approved` and is spent, as under the
/// current name.
#[tokio::test]
async fn test_the_pre_rename_preauth_key_field_is_accepted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await?["status"], "approved");

    assert!(
        state
            .client_store
            .list_client_preauth_keys()
            .await?
            .is_empty(),
        "the aliased field must spend the key, not merely be tolerated"
    );
    Ok(())
}

/// Serde's duplicate-field `400`, emergent from the alias: nothing in the
/// handler would notice if a serde upgrade let the second credential win.
#[tokio::test]
async fn test_both_client_preauth_key_spellings_at_once_are_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    let pk = fresh_pubkey()?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": token,
            "preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(pk)?)
            .await?,
        "a refused body must not create a client"
    );

    // The refusal lands before the consume.
    assert_eq!(
        state.client_store.list_client_preauth_keys().await?.len(),
        1
    );
    Ok(())
}

/// The prefix says which build handed the token out, not which key it names.
#[rstest]
#[case::current_name("client_preauth_key")]
#[case::pre_rename_name("preauth_key")]
#[tokio::test]
async fn test_the_legacy_client_preauth_token_prefix_is_accepted(
    #[case] field: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::MultiUse)).await?;
    let token_body = token
        .strip_prefix(pipette_core::client_preauth::TOKEN_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("mint must use the current prefix"))?;

    let mut body = json!({
        "public_key": fresh_pubkey()?,
        "organization": "o",
        "client_details": "d",
        "contact_email": "a@b.com",
    });
    body[field] = json!(format!(
        "{}{token_body}",
        pipette_core::client_preauth::LEGACY_TOKEN_PREFIX
    ));

    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await?["status"], "approved");
    Ok(())
}

#[tokio::test]
async fn test_single_use_client_preauth_key_consumed_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    let register = |pk: String, token: String| {
        let state = &state;
        async move {
            unauthed_post(
                state,
                "/clients/register",
                &json!({
                    "public_key": pk,
                    "organization": "o",
                    "client_details": "d",
                    "contact_email": "a@b.com",
                    "client_preauth_key": token,
                }),
            )
            .await
        }
    };

    let first = register(fresh_pubkey()?, token.clone()).await?;
    assert_eq!(first.status(), StatusCode::CREATED);
    // A spent single-use key deletes itself, so nothing lingers to prune.
    assert!(
        state
            .client_store
            .list_client_preauth_keys()
            .await?
            .is_empty(),
        "single-use key should be gone after consume"
    );
    let second = register(fresh_pubkey()?, token).await?;
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Settled by an exclusive create, not arrival order; else one key mints as
/// many approved clients as a holder asks for in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_use_client_preauth_key_admits_one_of_many_concurrent_registrations()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    // Distinct keypairs: a repeated one takes the idempotent path, which
    // returns before the pre-auth key is examined.
    let keys = (0..16)
        .map(|_| fresh_pubkey())
        .collect::<anyhow::Result<Vec<_>>>()?;
    let sent = futures::future::join_all(keys.into_iter().map(|public_key| {
        let token = token.clone();
        let state = &state;
        async move {
            unauthed_post(
                state,
                "/clients/register",
                &json!({
                    "public_key": public_key,
                    "organization": "o",
                    "client_details": "d",
                    "contact_email": "a@b.com",
                    "client_preauth_key": token,
                }),
            )
            .await
        }
    }))
    .await;

    let statuses = sent
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .map(|resp| resp.status())
        .collect::<Vec<_>>();

    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count(),
        1,
        "exactly one registration may spend the key; got {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::CREATED || *s == StatusCode::UNAUTHORIZED),
        "a losing registration is rejected, not errored; got {statuses:?}"
    );
    Ok(())
}

/// The marker, not the record's absence, holds a key spent, so a crash between
/// the two writes cannot revive it.
#[tokio::test]
async fn test_a_key_stays_spent_when_its_record_survives_the_spend() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let minted = pipette_core::client_preauth::mint(
        key_params(ClientPreauthUsage::SingleUse),
        chrono::Utc::now(),
    )?;
    state
        .client_store
        .put_client_preauth_key(&minted.key)
        .await?;

    let register = |public_key: String| {
        let state = &state;
        let token = minted.token.expose().to_string();
        async move {
            unauthed_post(
                state,
                "/clients/register",
                &json!({
                    "public_key": public_key,
                    "organization": "o",
                    "client_details": "d",
                    "contact_email": "a@b.com",
                    "client_preauth_key": token,
                }),
            )
            .await
        }
    };

    assert_eq!(
        register(fresh_pubkey()?).await?.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        state
            .client_store
            .list_spent_client_preauth_markers()
            .await?
            .len(),
        1,
        "spending records a marker"
    );

    // Stands in for a spend whose record delete failed.
    state
        .client_store
        .put_client_preauth_key(&minted.key)
        .await?;
    assert_eq!(
        register(fresh_pubkey()?).await?.status(),
        StatusCode::UNAUTHORIZED,
        "a readable record does not make a spent key consumable"
    );
    Ok(())
}

enum RefusedBeforeClaim {
    MissingRequiredKey,
    MalformedKey,
    UnknownKey,
    WrongSecret,
    ExpiredKey,
    /// A key minted over HTTP by an account that no longer exists.
    MinterGone,
    /// The stored key's own id and secret, under the user keyspace's prefix.
    UsersToken,
}

enum NextRegistration {
    User,
    Client,
}

/// The route is unauthenticated, so a claim left here would let anyone who
/// knows a public key bar it from user registration. A wrong secret reads like
/// an unknown key, so the route is no oracle for which key ids exist.
#[rstest]
#[case::missing_required_key(
    RefusedBeforeClaim::MissingRequiredKey,
    StatusCode::FORBIDDEN,
    "registration requires a pre-auth key"
)]
#[case::malformed_key(
    RefusedBeforeClaim::MalformedKey,
    StatusCode::UNAUTHORIZED,
    "malformed pre-auth key"
)]
#[case::users_token(
    RefusedBeforeClaim::UsersToken,
    StatusCode::UNAUTHORIZED,
    "malformed pre-auth key"
)]
#[case::unknown_key(
    RefusedBeforeClaim::UnknownKey,
    StatusCode::UNAUTHORIZED,
    "invalid pre-auth key"
)]
#[case::wrong_secret(
    RefusedBeforeClaim::WrongSecret,
    StatusCode::UNAUTHORIZED,
    "invalid pre-auth key"
)]
#[case::expired_key(
    RefusedBeforeClaim::ExpiredKey,
    StatusCode::UNAUTHORIZED,
    "pre-auth key has expired"
)]
#[case::minter_gone(
    RefusedBeforeClaim::MinterGone,
    StatusCode::FORBIDDEN,
    "this pre-auth key no longer admits a device; ask the organization's administrator for a new one"
)]
#[tokio::test]
async fn test_a_refused_client_preauth_key_leaves_the_keypair_unclaimed(
    #[case] refusal: RefusedBeforeClaim,
    #[case] status: StatusCode,
    #[case] want: &str,
    #[values(NextRegistration::User, NextRegistration::Client)] next: NextRegistration,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = match refusal {
        RefusedBeforeClaim::MissingRequiredKey => {
            make_state_require_client_preauth(dir.path()).await?
        }
        RefusedBeforeClaim::MalformedKey
        | RefusedBeforeClaim::UnknownKey
        | RefusedBeforeClaim::WrongSecret
        | RefusedBeforeClaim::ExpiredKey
        | RefusedBeforeClaim::MinterGone
        | RefusedBeforeClaim::UsersToken => make_state(dir.path()).await?,
    };
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;
    let mut expired = key_params(ClientPreauthUsage::SingleUse);
    expired.expires_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));
    let expired = mint_stored_key(&state, expired).await?;
    let mut delegated = pipette_core::client_preauth::mint(
        key_params(ClientPreauthUsage::SingleUse),
        chrono::Utc::now(),
    )?;
    delegated.key.minted_by = Some(UserId::mint());
    state
        .client_store
        .put_client_preauth_key(&delegated.key)
        .await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let public_key = PublicKeyHex::try_new(hex::encode(signing_key.verifying_key().as_bytes()))?;
    let mut body = json!({
        "public_key": public_key.as_str(),
        "organization": "o",
        "client_details": "d",
        "contact_email": "lab@example.test",
    });
    let presented = match refusal {
        RefusedBeforeClaim::MissingRequiredKey => None,
        RefusedBeforeClaim::MalformedKey => Some("not-a-token".to_string()),
        RefusedBeforeClaim::UnknownKey => {
            Some("client_preauth_deadbeefdeadbeef.notarealsecret".to_string())
        }
        RefusedBeforeClaim::WrongSecret => {
            let (key_id, _) = token
                .rsplit_once('.')
                .ok_or_else(|| anyhow::anyhow!("a token is id.secret"))?;
            Some(format!("{key_id}.notthesecret"))
        }
        RefusedBeforeClaim::ExpiredKey => Some(expired),
        RefusedBeforeClaim::MinterGone => Some(delegated.token.expose().to_string()),
        RefusedBeforeClaim::UsersToken => Some(format!(
            "{}{}",
            pipette_core::user_preauth::TOKEN_PREFIX,
            token
                .strip_prefix(pipette_core::client_preauth::TOKEN_PREFIX)
                .ok_or_else(|| anyhow::anyhow!("mint must use the current prefix"))?
        )),
    };
    if let Some(presented) = presented {
        body["client_preauth_key"] = json!(presented);
    }

    let resp = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(resp.status(), status);
    assert_eq!(body_json(resp).await?, json!({ "error": want }));
    assert_eq!(
        state
            .key_index_store
            .holder_of_public_key(&digest_of(&public_key))
            .await?,
        None,
        "a refused registration must not claim the keypair"
    );
    // The single-use, expired and delegated keys all survive, and none is marked spent.
    assert_eq!(
        state.client_store.list_client_preauth_keys().await?.len(),
        3,
        "a refusal must not spend a stored key"
    );
    assert!(
        state
            .client_store
            .list_spent_client_preauth_markers()
            .await?
            .is_empty(),
        "a refusal must not mark a stored key spent"
    );

    match next {
        NextRegistration::User => {
            let resp =
                request_user_account(&state, "owner@example.test", &signing_key, "laptop").await?;
            assert_eq!(
                resp.status(),
                StatusCode::CREATED,
                "the keypair is free to become a user key"
            );
        }
        NextRegistration::Client => {
            body["client_preauth_key"] = json!(token);
            let resp = unauthed_post(&state, "/clients/register", &body).await?;
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert_eq!(body_json(resp).await?["status"], "approved");
            assert_eq!(
                state.client_store.list_client_preauth_keys().await?.len(),
                2,
                "the admitted registration spends the single-use key"
            );
        }
    }
    Ok(())
}

// A client that registered but failed to save the result retries with the same
// keypair and its spent token: the idempotent path never re-examines the key.
#[tokio::test]
async fn test_reregister_same_key_after_client_preauth_spent_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());
    let body = json!({
        "public_key": pk_hex,
        "organization": "o",
        "client_details": "d",
        "contact_email": "a@b.com",
        "client_preauth_key": token,
    });

    let first = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await?;
    assert_eq!(first_body["status"], "approved");
    assert!(
        state
            .client_store
            .list_client_preauth_keys()
            .await?
            .is_empty(),
        "single-use key should be spent after the first register"
    );

    let retry = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(retry.status(), StatusCode::OK);
    let retry_body = body_json(retry).await?;
    assert_eq!(retry_body["client_id"], first_body["client_id"]);
    assert_eq!(retry_body["status"], "approved");
    Ok(())
}

// Approval is `clients approve` or a first registration's key, not a repeat,
// and a repeat must never waste a key.
#[tokio::test]
async fn test_reregister_pending_client_with_key_is_noop() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let first = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
        }),
    )
    .await?;
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(body_json(first).await?["status"], "pending");

    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;
    let repeat = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "client_preauth_key": token,
        }),
    )
    .await?;

    assert_eq!(repeat.status(), StatusCode::OK);
    assert_eq!(body_json(repeat).await?["status"], "pending");
    assert_eq!(
        state.client_store.list_client_preauth_keys().await?.len(),
        1,
        "re-registration must not consume the pre-auth key"
    );
    Ok(())
}

#[tokio::test]
async fn test_multi_use_client_preauth_key_accepts_repeated_registration() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::MultiUse)).await?;

    let register = |token: String| {
        let state = &state;
        async move {
            unauthed_post(
                state,
                "/clients/register",
                &json!({
                    "public_key": fresh_pubkey()?,
                    "organization": "o",
                    "client_details": "d",
                    "contact_email": "a@b.com",
                    "client_preauth_key": token,
                }),
            )
            .await
        }
    };

    assert_eq!(register(token.clone()).await?.status(), StatusCode::CREATED);
    assert_eq!(register(token.clone()).await?.status(), StatusCode::CREATED);
    assert_eq!(register(token).await?.status(), StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn test_require_client_preauth_key_rejects_keyless_registration() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_require_client_preauth(dir.path()).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// A keyless registration matching an auto-approve rule is still refused, and
/// creates nothing.
#[tokio::test]
async fn test_require_client_preauth_key_outranks_auto_approve() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.require_client_preauth_key = true;
    config.auto_approve = AutoApproveConfig {
        emails: vec!["lab@example.test".to_string()],
        domains: vec!["example.test".to_string()],
    };
    let state = make_state_from_config(config).await?;

    let (status, body, pk) = register_fresh(&state, None).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        json!({ "error": "registration requires a pre-auth key" })
    );
    assert!(
        !state
            .client_store
            .has_public_key(&PublicKeyHex::try_new(pk)?)
            .await?,
        "a refused registration must not create a client"
    );
    Ok(())
}

/// One keyless registration, a device's when `n` is even and an account's when odd.
async fn keyless_registration(
    state: &AppState,
    n: u32,
) -> anyhow::Result<(PublicKeyHex, StatusCode)> {
    let (public_key, resp) = keyless_registration_from(state, n, &[]).await?;
    Ok((public_key, resp.status()))
}

async fn keyless_registration_from(
    state: &AppState,
    n: u32,
    headers: &[(&str, String)],
) -> anyhow::Result<(PublicKeyHex, axum::http::Response<axum::body::Body>)> {
    let public_key = PublicKeyHex::try_new(fresh_pubkey()?)?;
    let (uri, body) = if n.is_multiple_of(2) {
        (
            "/clients/register",
            json!({
                "public_key": public_key.as_str(),
                "organization": "TestOrg",
                "client_details": "flood",
                "contact_email": "flood@test.com"
            }),
        )
    } else {
        (
            "/api/v1/users/register",
            json!({
                "public_key": public_key.as_str(),
                "contact_email": format!("flood{n}@test.com"),
                "key_details": "flood"
            }),
        )
    };
    let resp = json_request(state, "POST", uri, headers, &body).await?;
    Ok((public_key, resp))
}

#[tokio::test]
async fn keyless_registrations_past_the_ceiling_are_refused_but_a_preauth_key_is_not()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    anyhow::ensure!(
        KEYLESS_REGISTRATIONS_PER_WINDOW.is_multiple_of(2),
        "an even ceiling makes the two refused registrations a device then an account"
    );
    let sent: Vec<(PublicKeyHex, StatusCode)> =
        futures::stream::iter(0..KEYLESS_REGISTRATIONS_PER_WINDOW + 2)
            .then(|n| keyless_registration(&state, n))
            .try_collect()
            .await?;
    let (admitted, refused) = sent
        .split_at_checked(usize::try_from(KEYLESS_REGISTRATIONS_PER_WINDOW)?)
        .context("every registration was sent")?;
    let [(device_key, device), (account_key, account)] = refused else {
        anyhow::bail!("expected two refused registrations, got {refused:?}");
    };
    assert!(
        admitted
            .iter()
            .all(|(_, status)| *status == StatusCode::CREATED),
        "{admitted:?}"
    );
    assert_eq!(
        (*device, *account),
        (StatusCode::TOO_MANY_REQUESTS, StatusCode::TOO_MANY_REQUESTS)
    );
    assert!(
        state
            .client_store
            .get_client(&ClientId::derive(device_key))
            .await?
            .is_none(),
        "a refused device must not leave a client record"
    );
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email(&format!(
                "flood{}@test.com",
                KEYLESS_REGISTRATIONS_PER_WINDOW + 1
            ))
            .await?,
        None,
        "a refused account must not claim its address"
    );
    let key_index = &state.key_index_store;
    let holders: Vec<Option<KeyHolderPrincipalId>> =
        futures::stream::iter([device_key, account_key])
            .then(|key| async move { key_index.holder_of_public_key(&digest_of(key)).await })
            .try_collect()
            .await?;
    assert_eq!(
        holders,
        [None, None],
        "a refused registration must not claim its keypair"
    );

    let token = mint_stored_key(&state, key_params(ClientPreauthUsage::SingleUse)).await?;
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey()?,
            "organization": "TestOrg",
            "client_details": "keyed",
            "contact_email": "keyed@test.com",
            "client_preauth_key": token,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn one_client_past_its_share_is_refused_with_a_wait_but_others_are_not() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Each varies the entry left of the load balancer's, which counts for nothing.
    let from = |client: &str, n: u32| [("x-forwarded-for", format!("10.9.9.{n}, {client}"))];
    let state = &state;
    let admitted: Vec<StatusCode> = futures::stream::iter(0..KEYLESS_REGISTRATIONS_PER_CLIENT)
        .then(|n| async move {
            let (_, resp) = keyless_registration_from(state, n, &from("203.0.113.7", n)).await?;
            anyhow::Ok(resp.status())
        })
        .try_collect()
        .await?;
    assert!(
        admitted.iter().all(|status| *status == StatusCode::CREATED),
        "{admitted:?}"
    );

    let n = KEYLESS_REGISTRATIONS_PER_CLIENT;
    let (public_key, resp) = keyless_registration_from(state, n, &from("203.0.113.7", n)).await?;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = resp
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .context("a 429 carries Retry-After")?
        .to_str()?
        .parse()?;
    assert!((1..=61).contains(&wait), "Retry-After: {wait}");
    let holder = state
        .key_index_store
        .holder_of_public_key(&digest_of(&public_key))
        .await?;
    assert_eq!(
        holder, None,
        "a refused registration must not claim its keypair"
    );

    let (_, resp) = keyless_registration_from(state, n, &from("198.51.100.1", n)).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}
