mod helpers;

use axum::http::StatusCode;
use ed25519_dalek::Signer;
use rstest::rstest;

use helpers::{
    make_state, make_state_with_legacy_signatures, register_and_approve, setup_benchmarks,
    unauthed_get,
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

#[tokio::test]
async fn test_auth_rejected_with_expired_timestamp() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let expired = "2020-01-01T00:00:00Z".to_string();
    let headers = helpers::auth_headers_at(&sk, &client_id, "GET", "/clients/me", expired);
    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// A signature is minted for one exact request: it verifies for that request and
/// is rejected against a different path, method, or query string. That binding
/// is the whole reason the payload covers more than the timestamp.
///
/// The accepting cases are what keep the rejecting ones honest — a rejection
/// proves nothing on its own, since it would also occur if the server never
/// assembled the payload it documents. `signed_request_with_query` additionally
/// pins that the query string reaches the payload verbatim rather than being
/// dropped or normalized.
///
/// Every target here is an authenticated route, so a rejection proves the
/// signature check ran. Pointing a case at an open route (`GET /benchmarks`
/// serves unauthenticated callers) would pass without verifying anything.
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
    let headers = helpers::auth_headers(&sk, &client_id, "GET", signed_path);
    let resp = helpers::request_with_headers(&state, sent_method, sent_path, &headers).await?;

    assert_eq!(resp.status(), expected);
    Ok(())
}

/// A timestamp-only signature is accepted only while `accept_legacy_signatures`
/// is set, so the compatibility window closes on a config change alone. `v1`
/// signatures verify on either setting, so clearing the flag needs no
/// accompanying client-side change.
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
        helpers::legacy_auth_headers(&sk, &client_id)
    } else {
        helpers::auth_headers(&sk, &client_id, "GET", "/clients/me")
    };
    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), expected);
    Ok(())
}

/// Two requests identical in every signed field but the nonce, sent within one
/// timestamp's resolution. Resending the captured headers verbatim is the
/// replay, and it is rejected; re-signing the same request is a reissue, and it
/// is accepted. The nonce is the only thing separating the two, which is why a
/// signature keyed on the request *shape* alone could not tell them apart.
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
    let captured =
        helpers::auth_headers_at(&sk, &client_id, "GET", "/clients/me", timestamp.clone());
    let second = if resend_captured_headers {
        captured.clone()
    } else {
        helpers::auth_headers_at(&sk, &client_id, "GET", "/clients/me", timestamp)
    };

    let first = helpers::authed_get_with_headers(&state, "/clients/me", &captured).await?;
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "the capture must itself pass"
    );

    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &second).await?;
    assert_eq!(resp.status(), expected);
    Ok(())
}

/// A `v1` signature without the header its payload covers cannot verify, and
/// the rejection names the nonce rather than reporting a bare signature
/// mismatch. Legacy acceptance is off so the timestamp-only fallback cannot
/// mask the failure.
#[tokio::test]
async fn test_missing_nonce_is_rejected_by_name() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), false).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let headers: Vec<(&str, String)> = helpers::auth_headers(&sk, &client_id, "GET", "/clients/me")
        .into_iter()
        .filter(|(name, _)| *name != "X-Nonce")
        .collect();
    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = helpers::body_json(resp).await?;
    assert_eq!(helpers::json_str(&body, "error")?, "missing X-Nonce header");
    Ok(())
}

/// A client registered with a small-order public key is signed for by nobody:
/// the forged signature below carries no proof of a private key, and both
/// signed payloads reject it whatever `accept_legacy_signatures` says.
///
/// The forgery verifies for any message under lenient verification — see
/// `test_a_small_order_key_forges_a_signature_for_any_message` in `src/auth.rs`
/// for that proof — so the rejection here is the server's strictness and not an
/// incidental parse failure. Registration accepts the key because
/// `PublicKeyHex` checks encoding rather than curve structure, which is what
/// puts the burden on verification.
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
        helpers::register_and_approve_public_key(&state, &hex::encode(identity)).await?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(forged)),
        ("X-Nonce", "0123456789abcdef0123456789abcdef".to_string()),
    ];

    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = helpers::body_json(resp).await?;
    assert_eq!(helpers::json_str(&body, "error")?, "invalid signature");
    Ok(())
}

/// A registered public key that encodes no curve point is the caller's problem,
/// not the service's, so presenting it draws a `401` rather than a `500`.
/// Registration admits the key — `PublicKeyHex` checks hex and length, and
/// about half of all 32-byte strings decode to no point — so without this the
/// route would be an unauthenticated way to mint server errors and the
/// `error!`-level records that accompany them.
#[tokio::test]
async fn test_an_off_curve_public_key_is_the_callers_fault() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let off_curve = [0x02u8; 32];
    let client_id =
        helpers::register_and_approve_public_key(&state, &hex::encode(off_curve)).await?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode([0u8; 64])),
        ("X-Nonce", "0123456789abcdef0123456789abcdef".to_string()),
    ];

    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = helpers::body_json(resp).await?;
    assert_eq!(helpers::json_str(&body, "error")?, "invalid public key");
    Ok(())
}

/// The fallback is withdrawn the moment a client proves it no longer needs it.
/// A client may sign timestamp-only until it presents a `v1` signature; from
/// then on the same timestamp-only request it was making before is refused,
/// with `accept_legacy_signatures` left on throughout — so it is the client's
/// own migration that closes the path, not a config change.
///
/// This is what makes signatures captured before the migration worthless: they
/// carry no nonce, so the replay cache cannot spend them, and this is the only
/// thing standing between a captured one and a replay.
#[tokio::test]
async fn test_a_v1_signature_withdraws_the_clients_timestamp_only_fallback() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let legacy = helpers::legacy_auth_headers(&sk, &client_id);
    let before = helpers::authed_get_with_headers(&state, "/clients/me", &legacy).await?;
    assert_eq!(
        before.status(),
        StatusCode::OK,
        "a client that has never sent v1 may still sign timestamp-only"
    );

    let v1 = helpers::auth_headers(&sk, &client_id, "GET", "/clients/me");
    let migrating = helpers::authed_get_with_headers(&state, "/clients/me", &v1).await?;
    assert_eq!(migrating.status(), StatusCode::OK);

    let legacy_again = helpers::legacy_auth_headers(&sk, &client_id);
    let after = helpers::authed_get_with_headers(&state, "/clients/me", &legacy_again).await?;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    let body = helpers::body_json(after).await?;
    assert_eq!(helpers::json_str(&body, "error")?, "invalid signature");

    // The client itself is unaffected — only the fallback was withdrawn.
    let still_v1 = helpers::auth_headers(&sk, &client_id, "GET", "/clients/me");
    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &still_v1).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// The refusal outlives the process that observed the migration. The in-memory
/// cache holds only the migrated direction, so a restart must fall back to the
/// store rather than treating an unknown client as un-migrated — otherwise a
/// captured signature would work again after every deploy.
#[tokio::test]
async fn test_the_withdrawn_fallback_survives_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let v1 = helpers::auth_headers(&sk, &client_id, "GET", "/clients/me");
    assert_eq!(
        helpers::authed_get_with_headers(&state, "/clients/me", &v1)
            .await?
            .status(),
        StatusCode::OK
    );

    // A second state over the same storage: new caches, same durable markers.
    let restarted = make_state_with_legacy_signatures(dir.path(), true).await?;
    let legacy = helpers::legacy_auth_headers(&sk, &client_id);
    let resp = helpers::authed_get_with_headers(&restarted, "/clients/me", &legacy).await?;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Withdrawing the fallback takes the client's private key, not merely a `v1`
/// request naming it. A client id travels in a plaintext header and appears in
/// the operator listing, so it is known to anyone who has seen the client's
/// traffic; the ratchet is one-way, with no path back short of reaching into
/// the store by hand. Recording the migration on the attempt rather than on the
/// proof would therefore let a stranger strand any client it can name — refused
/// the fallback its software still depends on, permanently.
#[tokio::test]
async fn test_an_unverified_v1_attempt_leaves_the_fallback_intact() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_legacy_signatures(dir.path(), true).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // A well-formed `v1` request naming the victim, signed by a key the victim
    // does not hold — the strongest forgery someone knowing only the client id
    // can put together.
    let attacker = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
    let forged = helpers::auth_headers(&attacker, &client_id, "GET", "/clients/me");
    let rejected = helpers::authed_get_with_headers(&state, "/clients/me", &forged).await?;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let legacy = helpers::legacy_auth_headers(&sk, &client_id);
    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &legacy).await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a `v1` signature that failed to verify must leave the fallback in place"
    );
    Ok(())
}

/// Pins the exact bytes a client signs. The literal below is the wire contract:
/// changing it invalidates the signatures of every deployed client, so it must
/// be a deliberate version bump (`v1` → `v2`, served alongside `v1` through the
/// rollout) rather than an incidental edit. Written out in full here — not
/// built from a shared helper — so that a change on either side fails loudly.
#[tokio::test]
async fn test_signed_payload_wire_format() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let nonce = "0123456789abcdef0123456789abcdef";
    let payload = format!("v1\nGET\n/clients/me\n{timestamp}\n{client_id}\n{nonce}");
    let headers = vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        (
            "X-Signature",
            hex::encode(sk.sign(payload.as_bytes()).to_bytes()),
        ),
        ("X-Nonce", nonce.to_string()),
    ];

    let resp = helpers::authed_get_with_headers(&state, "/clients/me", &headers).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}
