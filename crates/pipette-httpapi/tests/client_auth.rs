//! Request authentication for a **client**: the `v1` and `v3` signed payloads
//! and every way of getting them wrong. A user signs `v2` and shares none of this.

use anyhow::Context;
use axum::http::StatusCode;
use ed25519_dalek::{Signer, SigningKey};
use rstest::rstest;

use pipette_core::types::DeriveFromKey;
use pipette_core::user_permission::UserPermissionGroup;
use pipette_plan_types::{ClientId, PublicKeyHex, UserPermission};
use pipette_web_testkit::{
    TEST_AUTHORITY, UserSigner, make_state, make_state_from_config,
    make_state_with_legacy_signatures, register_and_approve, register_pending, setup_benchmarks,
    test_config, unauthed_get,
};

#[tokio::test]
async fn test_auth_rejected_without_headers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/clients/me").await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// The freshness window holds in both directions, so a request dated ahead of
/// the server is refused like a stale one. A one-sided check would accept a
/// future-dated signature again once the replay cache sweeps it out.
#[rstest]
#[case::in_the_past(-6)]
#[case::in_the_future(6)]
#[tokio::test]
async fn test_auth_rejected_with_expired_timestamp(
    #[case] offset_minutes: i64,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let outside = (chrono::Utc::now() + chrono::Duration::minutes(offset_minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let headers =
        pipette_web_testkit::auth_headers_at(&sk, &client_id, "GET", "/clients/me", outside)?;
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        "timestamp expired"
    );
    Ok(())
}

/// Not looked up as an unknown client: the charset is what makes the id safe in
/// an object key and in the signed payload.
#[rstest]
#[case::a_dot(
    "ev1.bad",
    "invalid X-Client-Id header: client id contains '.'; allowed: [A-Za-z0-9_-]"
)]
#[case::a_path_separator(
    "ev1_a/b",
    "invalid X-Client-Id header: client id contains '/'; allowed: [A-Za-z0-9_-]"
)]
#[case::empty("", "invalid X-Client-Id header: client id must not be empty")]
#[tokio::test]
async fn test_a_malformed_client_id_is_refused_by_name(
    #[case] client_id: &str,
    #[case] expected: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode([0u8; 64])),
        ("X-Nonce", "0123456789abcdef0123456789abcdef".to_string()),
    ];
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(pipette_web_testkit::json_str(&body, "error")?, expected);
    Ok(())
}

/// Every route only a device signs for, by method and path (the vault and
/// catalog reads also serve users). The ids are well formed and name nothing.
const DEVICE_ROUTES: &[(&str, &str)] = &[
    ("GET", "/clients/me"),
    ("PATCH", "/clients/me"),
    ("POST", "/benchmarks"),
    ("POST", "/benchmarks/batch"),
    ("GET", "/jobs/job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"),
    (
        "GET",
        "/jobs/job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e/eval-sample-results",
    ),
    ("POST", "/plans/claim"),
    (
        "PUT",
        "/plans/job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e/heartbeat",
    ),
    (
        "POST",
        "/plans/job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e/reclaim",
    ),
    ("GET", "/datasets/prefill_throughput_256"),
];

/// A signer whose `X-Client-Id` names no client record.
enum Caller {
    /// A device that made its key and never registered it.
    NeverRegistered,
    /// An approved device an operator deleted.
    Deleted,
    /// A pending device an operator rejected, which deletes its record.
    Rejected,
    /// A server admin: its id parses as a client id and names no client.
    User,
}

/// The key a [`Caller`] signs with.
enum Credential {
    Device(SigningKey, ClientId),
    User(UserSigner),
}

impl Credential {
    fn sign(&self, method: &str, path: &str) -> anyhow::Result<Vec<(&'static str, String)>> {
        Ok(match self {
            Self::Device(key, client_id) => {
                pipette_web_testkit::auth_headers(key, client_id, method, path)?
            }
            Self::User((key, user_id, key_id)) => {
                pipette_web_testkit::user_auth_headers(key, user_id, key_id, method, path)?
            }
        })
    }

    /// The client id the extractor looks up for this signer.
    fn client_id(&self) -> anyhow::Result<ClientId> {
        Ok(match self {
            Self::Device(_, client_id) => client_id.clone(),
            Self::User((_, user_id, _)) => ClientId::try_new(user_id.to_string())?,
        })
    }
}

/// Removing the record is how a device is revoked, and a user must never pass
/// as a device. Each key first authenticates somewhere, so the refusal is the
/// missing record's.
#[rstest]
#[case::never_registered(Caller::NeverRegistered)]
#[case::deleted(Caller::Deleted)]
#[case::rejected(Caller::Rejected)]
#[case::a_user(Caller::User)]
#[tokio::test]
async fn test_a_signer_with_no_client_record_is_refused_on_every_device_route(
    #[case] caller: Caller,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (credential, removal) = match caller {
        Caller::NeverRegistered => {
            // The id registration would derive from this key.
            let sk = pipette_web_testkit::signing_key()?;
            let public_key = PublicKeyHex::try_new(hex::encode(sk.verifying_key().as_bytes()))?;
            let client_id = ClientId::derive(&public_key);
            (Credential::Device(sk, client_id), None)
        }
        Caller::Deleted => {
            let (sk, client_id) = register_and_approve(&state).await?;
            let uri = format!("/api/v1/clients/{client_id}");
            (Credential::Device(sk, client_id), Some(("DELETE", uri)))
        }
        Caller::Rejected => {
            let (sk, client_id) = register_pending(&state).await?;
            let uri = format!("/api/v1/clients/{client_id}/reject");
            (Credential::Device(sk, client_id), Some(("POST", uri)))
        }
        Caller::User => {
            // Every user right, so no grant can stand in for a device record.
            let user = pipette_web_testkit::registered_user(
                &state,
                UserPermissionGroup::ServerAdmin.permissions(),
            )
            .await?;
            let (key, user_id, key_id) = &user;
            let own = pipette_web_testkit::authed_user_get(
                &state,
                key,
                user_id,
                key_id,
                "/api/v1/users/me",
            )
            .await?;
            assert_eq!(
                own.status(),
                StatusCode::OK,
                "the user signs for its own route"
            );
            (Credential::User(user), None)
        }
    };

    if let Some((method, uri)) = removal {
        let headers = credential.sign("GET", "/clients/me")?;
        let before =
            pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;
        assert_eq!(
            before.status(),
            StatusCode::OK,
            "the device authenticates until its record goes"
        );
        let admin =
            pipette_web_testkit::registered_user(&state, &[UserPermission::ManageAnyClients])
                .await?;
        let resp = pipette_web_testkit::signed_empty(&state, &admin, method, &uri).await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{method} {uri}");
    }

    for (method, path) in DEVICE_ROUTES {
        let headers = credential.sign(method, path)?;
        let resp =
            pipette_web_testkit::request_with_headers(&state, method, path, &headers).await?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        let body = pipette_web_testkit::body_json(resp).await?;
        assert_eq!(
            pipette_web_testkit::json_str(&body, "error")?,
            "client not found",
            "{method} {path}"
        );
    }

    assert!(
        state
            .client_store
            .get_client(&credential.client_id()?)
            .await?
            .is_none(),
        "no refused request writes a client record"
    );
    Ok(())
}

/// The accepting cases keep the rejecting ones honest: a rejection alone would
/// also pass if the payload were never assembled. Every target authenticates,
/// so a rejection proves the check ran (`GET /benchmarks` is open).
#[rstest]
#[case::different_path(
    "/clients/me",
    "GET",
    "/jobs/job-does-not-exist",
    StatusCode::UNAUTHORIZED
)]
#[case::different_method("/clients/me", "PATCH", "/clients/me", StatusCode::UNAUTHORIZED)]
#[case::different_query(
    "/clients/me?page=1",
    "GET",
    "/clients/me?page=2",
    StatusCode::UNAUTHORIZED
)]
#[case::signed_request("/clients/me", "GET", "/clients/me", StatusCode::OK)]
#[case::signed_request_with_query(
    "/clients/me?page=2",
    "GET",
    "/clients/me?page=2",
    StatusCode::OK
)]
#[tokio::test]
async fn test_signature_binds_method_path_and_query(
    #[case] signed_path: &str,
    #[case] sent_method: &str,
    #[case] sent_path: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Every case signs a GET; `different_method` sends a different verb.
    let headers = pipette_web_testkit::auth_headers(&sk, &client_id, "GET", signed_path)?;
    let resp =
        pipette_web_testkit::request_with_headers(&state, sent_method, sent_path, &headers).await?;

    assert_eq!(resp.status(), expected);
    Ok(())
}

/// The window closes on a config change alone; `v1` verifies on either setting,
/// so clearing the flag needs no client-side change.
#[rstest]
#[case::timestamp_only_while_enabled(true, true, StatusCode::OK)]
#[case::timestamp_only_once_disabled(true, false, StatusCode::UNAUTHORIZED)]
#[case::v1_while_enabled(false, true, StatusCode::OK)]
#[case::v1_once_disabled(false, false, StatusCode::OK)]
#[tokio::test]
async fn test_signature_kind_follows_config(
    #[case] timestamp_only: bool,
    #[case] accept_legacy: bool,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), accept_legacy).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let headers = if timestamp_only {
        pipette_web_testkit::legacy_auth_headers(&sk, &client_id)
    } else {
        pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?
    };
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), expected);
    Ok(())
}

/// Within one timestamp, only the nonce separates a replay (the captured
/// headers resent) from a reissue (the same request signed again).
#[rstest]
#[case::replayed(true, StatusCode::UNAUTHORIZED)]
#[case::reissued(false, StatusCode::OK)]
#[tokio::test]
async fn test_a_repeated_request_hinges_on_the_nonce(
    #[case] resend_captured_headers: bool,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let captured = pipette_web_testkit::auth_headers_at(
        &sk,
        &client_id,
        "GET",
        "/clients/me",
        timestamp.clone(),
    )?;
    let second = if resend_captured_headers {
        captured.clone()
    } else {
        pipette_web_testkit::auth_headers_at(&sk, &client_id, "GET", "/clients/me", timestamp)?
    };

    let first =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &captured).await?;
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "the capture must itself pass"
    );

    let resp = pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &second).await?;
    assert_eq!(resp.status(), expected);
    Ok(())
}

/// Not a bare signature mismatch. Legacy acceptance is off so the
/// timestamp-only fallback cannot mask the failure.
#[tokio::test]
async fn test_missing_nonce_is_rejected_by_name() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), false).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let headers: Vec<(&str, String)> =
        pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?
            .into_iter()
            .filter(|(name, _)| *name != "X-Nonce")
            .collect();
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        "missing X-Nonce header"
    );
    Ok(())
}

/// A request without its timestamp or its signature is told which header is
/// missing, so a client that drops one can tell that apart from a bad key.
#[rstest]
#[case::timestamp("X-Timestamp")]
#[case::signature("X-Signature")]
#[tokio::test]
async fn test_a_missing_credential_header_is_rejected_by_name(
    #[case] dropped: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let headers: Vec<(&str, String)> =
        pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?
            .into_iter()
            .filter(|(name, _)| *name != dropped)
            .collect();
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        format!("missing {dropped} header")
    );
    Ok(())
}

/// A blank nonce would make the signature name a request shape, not a request.
#[tokio::test]
async fn test_an_empty_nonce_is_rejected_as_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), false).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let payload = format!("v1\nGET\n/clients/me\n{timestamp}\n{client_id}\n");
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        (
            "X-Signature",
            hex::encode(sk.sign(payload.as_bytes()).to_bytes()),
        ),
        ("X-Nonce", String::new()),
    ];
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        "missing X-Nonce header"
    );
    Ok(())
}

/// The forgery verifies for any message under lenient verification (proved in
/// `src/auth.rs`), so this refusal is the server's strictness. Registration
/// admits the key: `PublicKeyHex` checks encoding, not curve structure.
#[rstest]
#[case::legacy_accepted(true)]
#[case::legacy_refused(false)]
#[tokio::test]
async fn test_a_small_order_public_key_cannot_be_signed_for(
    #[case] accept_legacy: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), accept_legacy).await?;

    // The identity point as a public key, and `R` = identity with `s` = 0 as
    // its signature.
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut forged = [0u8; 64];
    forged[0] = 1;

    let client_id =
        pipette_web_testkit::register_and_approve_public_key(&state, &hex::encode(identity))
            .await?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(forged)),
        ("X-Nonce", "0123456789abcdef0123456789abcdef".to_string()),
    ];

    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        "invalid signature"
    );
    Ok(())
}

/// A `401`, not a `500`: registration admits such a key, so a `500` would let
/// anyone mint server errors and their `error!` records.
#[tokio::test]
async fn test_an_off_curve_public_key_is_the_callers_fault() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let off_curve = [0x02u8; 32];
    let client_id =
        pipette_web_testkit::register_and_approve_public_key(&state, &hex::encode(off_curve))
            .await?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode([0u8; 64])),
        ("X-Nonce", "0123456789abcdef0123456789abcdef".to_string()),
    ];

    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        "invalid public key"
    );
    Ok(())
}

/// Not a `500` or a bare `invalid signature`.
#[rstest]
#[case::a_unix_timestamp("X-Timestamp", "1700000000".to_string(), "invalid timestamp format")]
#[case::a_non_hex_signature("X-Signature", "zz".to_string(), "invalid signature hex")]
#[case::a_key_length_signature(
    "X-Signature",
    hex::encode([0u8; 32]),
    "invalid signature length"
)]
#[tokio::test]
async fn test_a_malformed_credential_is_rejected_by_name(
    #[case] header: &str,
    #[case] value: String,
    #[case] expected: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut headers = pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?;
    let slot = headers
        .iter_mut()
        .find(|(name, _)| *name == header)
        .context("the signer sends every credential header")?;
    slot.1 = value;
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(resp).await?;
    assert_eq!(pipette_web_testkit::json_str(&body, "error")?, expected);
    Ok(())
}

/// With `accept_legacy_signatures` on throughout. Pre-migration signatures
/// carry no nonce, so this alone stands between a captured one and a replay.
#[tokio::test]
async fn test_a_v1_signature_withdraws_the_clients_timestamp_only_fallback() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let legacy = pipette_web_testkit::legacy_auth_headers(&sk, &client_id);
    let before =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &legacy).await?;
    assert_eq!(
        before.status(),
        StatusCode::OK,
        "a client that has never sent v1 may still sign timestamp-only"
    );

    let v1 = pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?;
    let migrating =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &v1).await?;
    assert_eq!(migrating.status(), StatusCode::OK);

    let legacy_again = pipette_web_testkit::legacy_auth_headers(&sk, &client_id);
    let after =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &legacy_again).await?;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    let body = pipette_web_testkit::body_json(after).await?;
    assert_eq!(
        pipette_web_testkit::json_str(&body, "error")?,
        "invalid signature"
    );

    // The client itself is unaffected — only the fallback was withdrawn.
    let still_v1 = pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?;
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &still_v1).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// The in-memory cache holds only the migrated direction, so a restart must
/// read the store, or a captured signature would work again after every deploy.
#[tokio::test]
async fn test_the_withdrawn_fallback_survives_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let v1 = pipette_web_testkit::auth_headers(&sk, &client_id, "GET", "/clients/me")?;
    assert_eq!(
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &v1)
            .await?
            .status(),
        StatusCode::OK
    );

    // A second state over the same storage: new caches, same durable markers.
    let restarted = make_state_with_legacy_signatures(dir.path(), true).await?;
    let legacy = pipette_web_testkit::legacy_auth_headers(&sk, &client_id);
    let resp =
        pipette_web_testkit::authed_get_with_headers(&restarted, "/clients/me", &legacy).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// A client id is public and the ratchet one-way, so recording the attempt
/// rather than the proof would let a stranger strand any client it can name.
#[tokio::test]
async fn test_an_unverified_v1_attempt_leaves_the_fallback_intact() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // The strongest forgery someone knowing only the client id can make.
    let attacker = pipette_web_testkit::signing_key()?;
    let forged = pipette_web_testkit::auth_headers(&attacker, &client_id, "GET", "/clients/me")?;
    let rejected =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &forged).await?;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let legacy = pipette_web_testkit::legacy_auth_headers(&sk, &client_id);
    let resp = pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &legacy).await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a `v1` signature that failed to verify must leave the fallback in place"
    );
    Ok(())
}

/// The literal is the wire contract: changing it breaks every deployed client,
/// so it takes a version bump. Written out, not built by a shared helper, so a
/// change on either side fails. `v1` covers no host, so a device in the field
/// sending any `Host` passes even where no `authorities` are configured.
#[rstest]
#[case::no_host(&[TEST_AUTHORITY], None)]
#[case::foreign_host(&[TEST_AUTHORITY], Some("mgmt.other"))]
#[case::none_configured(&[], Some(TEST_AUTHORITY))]
#[tokio::test]
async fn test_signed_payload_wire_format(
    #[case] ours: &[&str],
    #[case] host: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.authorities = ours.iter().map(|a| a.to_string()).collect();
    let state = make_state_from_config(config).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let nonce = "0123456789abcdef0123456789abcdef";
    let payload = format!("v1\nGET\n/clients/me\n{timestamp}\n{client_id}\n{nonce}");
    let mut headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        (
            "X-Signature",
            hex::encode(sk.sign(payload.as_bytes()).to_bytes()),
        ),
        ("X-Nonce", nonce.to_string()),
    ];
    headers.extend(host.map(|host| ("Host", host.to_string())));

    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// The timestamp-only fallback covers no host either, so a `Host` outside an
/// empty `authorities` must not refuse it.
#[tokio::test]
async fn test_a_timestamp_only_signature_ignores_the_host() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.accept_legacy_signatures = true;
    config.authorities = Vec::new();
    let state = make_state_from_config(config).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut headers = pipette_web_testkit::legacy_auth_headers(&sk, &client_id);
    headers.push(("Host", "mgmt.other".to_string()));
    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// `v3` is `v1` with the authority, so a device's request signed for another
/// deployment holding the same key is refused here, as is one whose `Host` was
/// changed after signing. Written out, like the `v1` literal above.
#[rstest]
#[case::ours(&[TEST_AUTHORITY], TEST_AUTHORITY, TEST_AUTHORITY, None)]
#[case::any_case(&["Mgmt.Test"], "mgmt.test", "MGMT.test", None)]
#[case::another_deployment(&[TEST_AUTHORITY], "mgmt.other", "mgmt.other", Some("does not answer to"))]
#[case::another_port(&[TEST_AUTHORITY], "mgmt.test:8443", "mgmt.test:8443", Some("does not answer to"))]
#[case::none_configured(&[], TEST_AUTHORITY, TEST_AUTHORITY, Some("does not answer to"))]
#[case::host_changed(&[TEST_AUTHORITY, "mgmt.other"], TEST_AUTHORITY, "mgmt.other", Some("invalid signature"))]
#[tokio::test]
async fn test_a_v3_signature_is_held_to_its_authority(
    #[case] ours: &[&str],
    #[case] signed_for: &str,
    #[case] sent_to: &str,
    #[case] refused: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.authorities = ours.iter().map(|a| a.to_string()).collect();
    let state = make_state_from_config(config).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let nonce = "0123456789abcdef0123456789abcdef";
    let payload = format!("v3\nGET\n{signed_for}\n/clients/me\n{timestamp}\n{client_id}\n{nonce}");
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        (
            "X-Signature",
            hex::encode(sk.sign(payload.as_bytes()).to_bytes()),
        ),
        ("X-Nonce", nonce.to_string()),
        ("Host", sent_to.to_string()),
    ];

    let resp =
        pipette_web_testkit::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    match refused {
        None => assert_eq!(resp.status(), StatusCode::OK),
        Some(reason) => {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            let body = pipette_web_testkit::body_json(resp).await?;
            let error = pipette_web_testkit::json_str(&body, "error")?;
            assert!(error.contains(reason), "{error}");
        }
    }
    Ok(())
}
