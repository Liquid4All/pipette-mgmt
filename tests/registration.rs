mod helpers;

use axum::http::StatusCode;
use ed25519_dalek::SigningKey;
use rstest::rstest;
use serde_json::json;

use std::collections::BTreeSet;

use pipette_mgmt::handlers::AppState;
use pipette_mgmt::preauth::{MintParams, PreauthUsage};
use pipette_mgmt::types::ClientId;
use pipette_mgmt::validated::{NonEmptyTrimmedString, PublicKeyHex, Tag};

use helpers::{
    body_json, make_state, make_state_require_preauth, make_state_with_auto_approve,
    setup_benchmarks, unauthed_post,
};

/// Mint a key, persist it, and return the one-time token to present at register.
async fn mint_stored_key(state: &AppState, params: MintParams) -> anyhow::Result<String> {
    let minted = pipette_mgmt::preauth::mint(params, chrono::Utc::now())?;
    state.auth_store.put_preauth_key(&minted.key).await?;
    Ok(minted.token)
}

fn key_params(usage: PreauthUsage) -> MintParams {
    MintParams {
        usage,
        expires_at: None,
        default_tags: BTreeSet::new(),
        default_organization: None,
        note: None,
    }
}

/// A fresh Ed25519 public key hex for a register call.
fn fresh_pubkey() -> String {
    let sk = SigningKey::generate(&mut rand_core::OsRng);
    hex::encode(sk.verifying_key().as_bytes())
}

#[tokio::test]
async fn test_register_with_client_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

#[tokio::test]
async fn test_register_with_generated_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_post(
        &state,
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
    assert!(body["private_key"].as_str().is_some());
    Ok(())
}

/// A response carrying a private key says so in the log, since the credential's
/// confidentiality rests on the deployment rather than on anything the server
/// can check (operations.md §5.6). The client-supplied case is what keeps the
/// event worth reading: announcing every registration would name nothing.
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

    let (status, logs) = helpers::capture_logs(|rt| -> anyhow::Result<StatusCode> {
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
                body["public_key"] = json!(fresh_pubkey());
            }
            let resp = unauthed_post(&state, "/clients/register", &body).await?;
            Ok(resp.status())
        })
    });

    assert_eq!(status?, StatusCode::CREATED);
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

    // A repeat with the same public key is idempotent: 200 (nothing created),
    // returning the same client_id and status rather than 409.
    let second = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = body_json(second).await?;
    assert_eq!(second_body["client_id"], first_body["client_id"]);
    assert_eq!(second_body["status"], first_body["status"]);
    Ok(())
}

#[tokio::test]
async fn test_register_rejects_both_public_key_and_generate_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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
    let client_id = pipette_mgmt::types::ClientId::try_new(body["client_id"].as_str().unwrap())?;

    let stored = state
        .auth_store
        .get_client(&client_id)
        .await?
        .expect("client should be stored");
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

    // Persisted status must also be approved.
    let client_id = pipette_mgmt::types::ClientId::try_new(body["client_id"].as_str().unwrap())?;
    let stored = state
        .auth_store
        .get_client(&client_id)
        .await?
        .expect("client should be stored");
    assert_eq!(stored.status, pipette_mgmt::client::ClientStatus::Approved);
    Ok(())
}

#[tokio::test]
async fn test_register_auto_approves_matching_domain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state =
        make_state_with_auto_approve(dir.path(), vec![], vec!["example.org".to_string()]).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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
    let client_id = pipette_mgmt::types::ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;

    // The profile is persisted as flat `device_*` keys.
    let stored = state
        .auth_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client not stored"))?;
    let stored = serde_json::to_value(&stored)?;
    assert_eq!(stored["device_form_factor"], "laptop");
    assert_eq!(stored["device_ram_bytes"], 36_000_000_000u64);
    assert_eq!(stored["device_gpu_model"], "M3 Pro GPU");

    // A non-empty profile flags the client for an eligible-index reindex.
    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.iter().any(|(flagged, _)| flagged == &client_id));
    Ok(())
}

#[tokio::test]
async fn test_register_without_device_profile_does_not_flag_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
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

    // No profile and no capabilities → empty effective capability set → no
    // reindex flag.
    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_register_with_capabilities_persists_and_flags_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // Capabilities alone (no device_* profile) are enough to make the client
    // matchable, so registration must flag it for an eligible-index reindex.
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
    let client_id = pipette_mgmt::types::ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;

    // Capabilities round-trip in the stored record.
    let stored = state
        .auth_store
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

/// A device-profile validation failure at registration is a `400` whose body
/// names the offending field (httpapi.md §2.2.3). One case per rule: bad form
/// factor, and each `*_vram`/`os_version` field present without its required
/// companion.
#[rstest]
#[case::form_factor(json!({"device_form_factor": "spaceship"}), "must be one of")]
#[case::form_factor_empty(json!({"device_form_factor": ""}), "must be one of")]
#[case::gpu_vram(json!({"device_gpu_vram_bytes": 18_000_000_000u64}), "device_gpu_vram_bytes requires device_gpu_model")]
#[case::npu_vram(json!({"device_npu_vram_bytes": 8_000_000_000u64}), "device_npu_vram_bytes requires device_npu_model")]
#[case::os_version(json!({"device_os_version": "15.3"}), "device_os_version requires device_os_name")]
#[case::reserved_capability(json!({"capabilities": ["os:ios"]}), "reserved namespace")]
#[case::empty_capability(json!({"capabilities": [" "]}), "must not be empty")]
// Non-canonical spellings are rejected *before* the reserved check, so a client
// cannot smuggle a reserved flag past it (`OS:ios`, `" os:ios"`) or report an
// unmatchable mixed-case free-form flag.
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // Valid identity fields plus the invalid device fragment under test.
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

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // A field that fails its newtype validator (here `organization: ""`) is
    // rejected during extraction. The `ApiJson` extractor maps that to a `400`
    // carrying the uniform `{"error": ...}` envelope — not axum's default
    // `422` + plain-text body.
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

#[tokio::test]
async fn test_preauth_key_auto_approves_and_seeds_tags_org() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut p = key_params(PreauthUsage::SingleUse);
    p.default_tags = BTreeSet::from([Tag::try_new("team-mobile")?]);
    p.default_organization = Some(NonEmptyTrimmedString::try_new("seeded-org")?);
    let token = mint_stored_key(&state, p).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey(),
            "organization": "client-org",
            "client_details": "d",
            "contact_email": "a@b.com",
            "preauth_key": token,
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
            .auth_store
            .list_client_ids_by_tag(&Tag::try_new("team-mobile")?)
            .await?,
        vec![id.clone()]
    );
    let client = state
        .auth_store
        .get_client(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client missing"))?;
    assert_eq!(client.organization.as_str(), "seeded-org");
    Ok(())
}

#[tokio::test]
async fn test_invalid_preauth_key_rejected_and_no_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let pk = fresh_pubkey();
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "preauth_key": "preauth_deadbeefdeadbeef.notarealsecret",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !state
            .auth_store
            .has_public_key(&PublicKeyHex::try_new(pk)?)
            .await?,
        "no client should be created on rejection"
    );
    Ok(())
}

#[tokio::test]
async fn test_single_use_preauth_key_consumed_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(PreauthUsage::SingleUse)).await?;

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
                    "preauth_key": token,
                }),
            )
            .await
        }
    };

    let first = register(fresh_pubkey(), token.clone()).await?;
    assert_eq!(first.status(), StatusCode::CREATED);
    // A spent single-use key deletes itself, so nothing lingers to prune.
    assert!(
        state.auth_store.list_preauth_keys().await?.is_empty(),
        "single-use key should be gone after consume"
    );
    // Second use of the (now absent) key is rejected.
    let second = register(fresh_pubkey(), token).await?;
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Exactly one of many simultaneous registrations may spend a single-use key.
///
/// The winner is settled in storage by an exclusive create, not by the order
/// requests happen to arrive in. Without that, every request that reads the
/// record before any of them deletes it is granted, and one key mints as many
/// approved clients as a holder cares to ask for in parallel — each inheriting
/// the key's organization and tags.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_use_preauth_key_admits_one_of_many_concurrent_registrations()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(PreauthUsage::SingleUse)).await?;

    // Distinct keypairs per request. Reusing one public key would take the
    // idempotent-re-registration path, which short-circuits before the key is
    // examined at all, and the race would never be reached.
    let sent = futures::future::join_all((0..16).map(|_| {
        let token = token.clone();
        let public_key = fresh_pubkey();
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
                    "preauth_key": token,
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

/// The marker, not the record's absence, is what holds a key spent. A spend
/// whose record delete never landed — a crash between the two writes — must
/// leave the key unusable, and the surviving record must not revive it.
#[tokio::test]
async fn test_a_key_stays_spent_when_its_record_survives_the_spend() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let minted =
        pipette_mgmt::preauth::mint(key_params(PreauthUsage::SingleUse), chrono::Utc::now())?;
    state.auth_store.put_preauth_key(&minted.key).await?;

    let register = |public_key: String| {
        let state = &state;
        let token = minted.token.clone();
        async move {
            unauthed_post(
                state,
                "/clients/register",
                &json!({
                    "public_key": public_key,
                    "organization": "o",
                    "client_details": "d",
                    "contact_email": "a@b.com",
                    "preauth_key": token,
                }),
            )
            .await
        }
    };

    assert_eq!(
        register(fresh_pubkey()).await?.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        state.auth_store.list_spent_markers().await?.len(),
        1,
        "spending records a marker"
    );

    // Put the record back, standing in for a spend that recorded the marker and
    // then failed to delete it.
    state.auth_store.put_preauth_key(&minted.key).await?;
    assert_eq!(
        register(fresh_pubkey()).await?.status(),
        StatusCode::UNAUTHORIZED,
        "a readable record does not make a spent key consumable"
    );
    Ok(())
}

// The retry-safety guarantee behind registration: a client that registered
// server-side but failed to persist the result locally can retry with the
// *same* keypair and the (now spent) single-use token. Registration is
// idempotent on the public key, so the exhausted key is never re-examined — the
// retry returns the original client instead of a 401, needing no fresh key.
#[tokio::test]
async fn test_reregister_same_key_after_preauth_spent_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(PreauthUsage::SingleUse)).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());
    let body = json!({
        "public_key": pk_hex,
        "organization": "o",
        "client_details": "d",
        "contact_email": "a@b.com",
        "preauth_key": token,
    });

    // First register consumes the single-use key and auto-approves.
    let first = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await?;
    assert_eq!(first_body["status"], "approved");
    assert!(
        state.auth_store.list_preauth_keys().await?.is_empty(),
        "single-use key should be spent after the first register"
    );

    // Retry with the same keypair + spent token → idempotent 200, same client_id,
    // still approved. The exhausted key does not cause a 401.
    let retry = unauthed_post(&state, "/clients/register", &body).await?;
    assert_eq!(retry.status(), StatusCode::OK);
    let retry_body = body_json(retry).await?;
    assert_eq!(retry_body["client_id"], first_body["client_id"]);
    assert_eq!(retry_body["status"], "approved");
    Ok(())
}

// First-registration-wins: re-registering an already-pending client with a
// (valid, auto-approve) key returns it still pending and leaves the key
// unconsumed — approval is the `clients approve` / auto-approve path, not
// re-registration, and a repeat must never waste a key.
#[tokio::test]
async fn test_reregister_pending_client_with_key_is_noop() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let signing_key = SigningKey::generate(&mut rand_core::OsRng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    // Keyless first registration → pending.
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

    // Re-register the same keypair with a valid auto-approve key.
    let token = mint_stored_key(&state, key_params(PreauthUsage::SingleUse)).await?;
    let repeat = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
            "preauth_key": token,
        }),
    )
    .await?;

    // Idempotent: still pending, and the key was not consumed.
    assert_eq!(repeat.status(), StatusCode::OK);
    assert_eq!(body_json(repeat).await?["status"], "pending");
    assert_eq!(
        state.auth_store.list_preauth_keys().await?.len(),
        1,
        "re-registration must not consume the pre-auth key"
    );
    Ok(())
}

#[tokio::test]
async fn test_multi_use_preauth_key_accepts_repeated_registration() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let token = mint_stored_key(&state, key_params(PreauthUsage::MultiUse)).await?;

    // A multi-use key registers any number of distinct clients.
    let register = |token: String| {
        let state = &state;
        async move {
            unauthed_post(
                state,
                "/clients/register",
                &json!({
                    "public_key": fresh_pubkey(),
                    "organization": "o",
                    "client_details": "d",
                    "contact_email": "a@b.com",
                    "preauth_key": token,
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
async fn test_require_preauth_key_rejects_keyless_registration() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_require_preauth(dir.path()).await?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": fresh_pubkey(),
            "organization": "o",
            "client_details": "d",
            "contact_email": "a@b.com",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}
