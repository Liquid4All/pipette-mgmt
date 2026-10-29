//! What a Clerk sign-in (`POST /api/v1/users/register/clerk`) leaves in the log.
//!
//! Its own target, since a log capture needs a process to itself
//! (`pipette_web_testkit::logs`).

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

use anyhow::Context;
use axum::http::StatusCode;
use rstest::rstest;

use pipette_core::user_permission::{UserPermissionGroup, UserPermissionSet};
use pipette_web_testkit::{
    ClerkInstance, capture_logs_at_level, create_user, json_request, make_state_with_clerk,
    seed_org, signing_key,
};

const CLAIMS_LINE: &str = "checking a Clerk token's claims";

/// Register a key with `token` at a server trusting `clerk`, with `provision`
/// as its rules and each organization they name, at the binary's default
/// level: the status, and what was logged.
fn register(
    clerk: &ClerkInstance,
    provision: &[(&str, &str)],
    token: &str,
) -> anyhow::Result<(StatusCode, String)> {
    let dir = tempfile::tempdir()?;
    let (status, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let state = make_state_with_clerk(dir.path(), clerk, provision).await?;
            for (_, slug) in provision {
                seed_org(&state, slug).await?;
            }
            create_user(&state, "operator@example.test").await?;
            let body = serde_json::json!({
                "public_key": hex::encode(signing_key()?.verifying_key().as_bytes()),
                "key_details": "console app, a browser",
            });
            let resp = json_request(
                &state,
                "POST",
                "/api/v1/users/register/clerk",
                &[("Authorization", format!("Bearer {token}"))],
                &body,
            )
            .await?;
            anyhow::Ok(resp.status())
        })
    })?;
    Ok((status?, logs))
}

/// Once its signature verifies, a token's claims are logged as Clerk rendered
/// them, so a refusal reads against what the template sent; the token never
/// is, and the address only by its domain.
#[rstest]
#[case::registered("pipette-mgmt", StatusCode::CREATED)]
#[case::refused("another-app", StatusCode::UNAUTHORIZED)]
#[test]
fn a_verified_token_logs_its_claims(
    #[case] audience: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let clerk = ClerkInstance::new()?;
    let mut claims = clerk.claims("operator@example.test");
    claims["aud"] = serde_json::Value::from(audience);
    let token = clerk.token(&claims)?;

    let (registered, logs) = register(&clerk, &[], &token)?;
    assert_eq!(registered, status, "{logs}");
    let line = logs
        .lines()
        .find(|line| line.contains(CLAIMS_LINE))
        .with_context(|| format!("no claims line in:\n{logs}"))?;
    for claim in [
        format!(r#""aud":"{audience}""#),
        r#""azp":"https://console.example.test""#.to_string(),
        r#""email":"*@example.test""#.to_string(),
        r#""email_verified":true"#.to_string(),
        r#""iss":"https://clerk.example.test""#.to_string(),
        r#""sub":"user_2clerk""#.to_string(),
    ] {
        assert!(line.contains(&claim), "{claim} missing from {line}");
    }
    assert!(
        !logs.contains("operator@"),
        "the address is logged:\n{logs}"
    );
    assert!(!logs.contains(&token), "the token is logged:\n{logs}");
    Ok(())
}

/// A token whose signature does not verify logs only its refusal: until then
/// its claims are whatever the caller wrote.
#[test]
fn an_unverified_token_logs_no_claims() -> anyhow::Result<()> {
    let clerk = ClerkInstance::new()?;
    let forged = ClerkInstance::new()?.token(&clerk.claims("operator@example.test"))?;

    let (status, logs) = register(&clerk, &[], &forged)?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{logs}");
    assert!(logs.contains("signature does not verify"), "{logs}");
    assert!(!logs.contains(CLAIMS_LINE), "{logs}");
    Ok(())
}

/// A sign-in that provisions an account logs the names it stored, in the
/// `granted` field `PUT /api/v1/users/{user_id}/permissions` logs.
#[test]
fn a_provisioned_account_logs_the_names_it_was_granted() -> anyhow::Result<()> {
    let clerk = ClerkInstance::new()?;
    let token = clerk.token(&clerk.claims("new@example.test"))?;

    let (status, logs) = register(&clerk, &[("example.test", "acme")], &token)?;
    assert_eq!(status, StatusCode::CREATED, "{logs}");
    let line = logs
        .lines()
        .find(|line| line.contains("user provisioned"))
        .with_context(|| format!("no provisioning line in:\n{logs}"))?;
    let member =
        UserPermissionSet::from_iter(UserPermissionGroup::OrgMember.permissions().iter().copied())
            .normalized();
    let granted = format!("granted={:?}", member.names());
    assert!(line.contains(&granted), "{granted} missing from {line}");
    assert!(!logs.contains("new@"), "the address is logged:\n{logs}");
    Ok(())
}
