//! `POST /api/v1/users/register/clerk`: a key registered to the account a Clerk token
//! names, for a browser that holds none. The token's checks, the provision rules and
//! `allowed_domains` are pinned here.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Context;
use axum::http::StatusCode;
use rstest::rstest;
use serde_json::json;

use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    AuditAction, AuditActor, AuditTarget, NonEmptyTrimmedString, PublicKeyHex, UserAccountType,
    UserId, UserKeyId, UserPermission, UserStatus,
};
use pipette_store::Stores;
use pipette_store_fs::build_local_fs_stores;
use pipette_store_testkit::CapturingAuditStore;
use pipette_web_testkit::{
    ClerkInstance, authed_user_get, body_json, create_user, json_request, json_str,
    make_state_with_clerk, make_state_with_clerk_section, request_user_account, seed_org,
    set_user_status, setup_benchmarks, signing_key, test_config, test_http_client,
};

const ROUTE: &str = "/api/v1/users/register/clerk";

async fn register(
    state: &AppState,
    token: &str,
    body: &serde_json::Value,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    json_request(
        state,
        "POST",
        ROUTE,
        &[("Authorization", format!("Bearer {token}"))],
        body,
    )
    .await
}

fn fresh_key_body() -> anyhow::Result<serde_json::Value> {
    Ok(json!({
        "public_key": hex::encode(signing_key()?.verifying_key().as_bytes()),
        "key_details": "console app, a browser",
    }))
}

async fn register_with(
    state: &AppState,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    json_request(state, "POST", ROUTE, headers, &fresh_key_body()?).await
}

async fn register_token(
    state: &AppState,
    token: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    register(state, token, &fresh_key_body()?).await
}

async fn error_of(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    let body = body_json(resp).await?;
    Ok(json_str(&body, "error")?.to_string())
}

/// The key signs as the token's account and lapses after `CLERK_KEY_LIFETIME`.
#[tokio::test]
async fn test_a_clerk_token_registers_an_expiring_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    let user = create_user(&state, "operator@example.test").await?;
    let browser = signing_key()?;

    let body = json!({
        "public_key": hex::encode(browser.verifying_key().as_bytes()),
        "key_details": "console app, a browser",
    });
    crate::api_schema::assert_conforms("RegisterClerkUserKeyRequest", &body)?;
    let resp = register(
        &state,
        &clerk.token(&clerk.claims("operator@example.test"))?,
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let receipt = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserKeyRegistered", &receipt)?;
    assert_eq!(receipt["user_id"], json!(user.user_id.as_str()));
    let key_id = pipette_plan_types::UserKeyId::try_new(json_str(&receipt, "key_id")?)?;

    let resp =
        authed_user_get(&state, &browser, &user.user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK, "the key signs");
    let me = body_json(resp).await?;
    let key = me["keys"]
        .as_array()
        .and_then(|keys| keys.first())
        .ok_or_else(|| anyhow::anyhow!("the key is listed: {me}"))?;
    let expires_at = chrono::DateTime::parse_from_rfc3339(json_str(key, "expires_at")?)?;
    let lifetime = expires_at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    let wanted = pipette_httpapi::api_v1::users::clerk::CLERK_KEY_LIFETIME;
    assert!(
        lifetime <= wanted && lifetime > wanted - chrono::TimeDelta::minutes(1),
        "{lifetime}"
    );
    Ok(())
}

/// The body is checked before the token is read.
#[tokio::test]
async fn test_a_malformed_body_leaves_the_token_unspent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    create_user(&state, "operator@example.test").await?;
    let token = clerk.token(&clerk.claims("operator@example.test"))?;
    let public_key = hex::encode(signing_key()?.verifying_key().as_bytes());

    let extra = json!({ "public_key": public_key, "key_details": "x", "extra": 1 });
    assert!(
        crate::api_schema::assert_conforms("RegisterClerkUserKeyRequest", &extra).is_err(),
        "the schema refuses an unknown field, as the route does"
    );
    let resp = register(&state, &token, &extra).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = register(
        &state,
        &token,
        &json!({ "public_key": public_key, "key_details": "x" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn test_a_clerk_token_registers_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    create_user(&state, "operator@example.test").await?;
    let token = clerk.token(&clerk.claims("operator@example.test"))?;

    for expected in [StatusCode::CREATED, StatusCode::UNAUTHORIZED] {
        let body = json!({
            "public_key": hex::encode(signing_key()?.verifying_key().as_bytes()),
            "key_details": "x",
        });
        assert_eq!(register(&state, &token, &body).await?.status(), expected);
    }
    Ok(())
}

#[tokio::test]
async fn test_a_server_without_clerk_refuses() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    pipette_web_testkit::setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state(dir.path()).await?;

    let body = json!({
        "public_key": hex::encode(signing_key()?.verifying_key().as_bytes()),
        "key_details": "x",
    });
    let resp = register(&state, "token", &body).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Whatever case Clerk spells the verified address in.
#[tokio::test]
async fn test_a_clerk_token_registers_to_the_account_its_address_names() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    let user = create_user(&state, "operator@example.test").await?;

    let resp = register_token(
        &state,
        &clerk.token(&clerk.claims("Operator@Example.test"))?,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "user_id")?, user.user_id.as_str());
    Ok(())
}

/// A claim the template renders in its other shape still registers: the
/// verification as a string, as a quoted shortcode renders it, and `aud` as a list.
/// So does an `iat` a moment ahead, inside the clock skew Clerk's clock may have.
#[rstest]
#[case::verified_as_a_string("email_verified", json!("true"))]
#[case::audience_as_a_list("aud", json!(["another-app", "pipette-mgmt"]))]
#[case::issued_within_the_skew("iat", json!(chrono::Utc::now().timestamp() + 2))]
#[tokio::test]
async fn test_a_clerk_claim_in_its_other_shape_registers(
    #[case] claim: &str,
    #[case] value: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    let user = create_user(&state, "operator@example.test").await?;
    let mut claims = clerk.claims("operator@example.test");
    claims[claim] = value;

    let resp = register_token(&state, &clerk.token(&claims)?).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "user_id")?, user.user_id.as_str());
    Ok(())
}

/// The default template's `aud` no longer registers once another is named.
#[rstest]
#[case::the_configured_template("acme-console", StatusCode::CREATED)]
#[case::the_default_template("pipette-mgmt", StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn test_a_configured_jwt_template_is_the_audience_a_token_needs(
    #[case] audience: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::with_jwt_template("acme-console")?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    create_user(&state, "operator@example.test").await?;
    let mut claims = clerk.claims("operator@example.test");
    claims["aud"] = json!(audience);

    let resp = register_token(&state, &clerk.token(&claims)?).await?;
    assert_eq!(resp.status(), status, "{audience}");
    if status == StatusCode::UNAUTHORIZED {
        let error = error_of(resp).await?;
        assert!(error.contains("acme-console JWT template"), "{error}");
    }
    Ok(())
}

/// The new account is in the rule's organization, and the next registration
/// finds it.
#[tokio::test]
async fn test_a_provisioned_domain_registers_to_a_new_org_member_account() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    let acme = seed_org(&state, "acme").await?;

    let resp = register_token(&state, &clerk.token(&clerk.claims("new@example.test"))?).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let user_id = pipette_plan_types::UserId::try_new(json_str(&body, "user_id")?)?;

    let user = state
        .user_store
        .get_user(&user_id)
        .await?
        .context("the account exists")?;
    assert_eq!(user.contact_email.as_str(), "new@example.test");
    assert_eq!(user.status, UserStatus::Active);
    assert_eq!(
        state.org_store.get_membership(&user_id).await?,
        Some(acme.org_id)
    );
    let held = pipette_core::user_permission::effective(
        state.user_store.get_user_permissions(&user_id).await?,
        pipette_core::user_permission::Holder::of(&user),
    );
    let missing: Vec<_> = UserPermissionGroup::OrgMember
        .permissions()
        .iter()
        .filter(|permission| !held.holds(**permission))
        .collect();
    assert!(missing.is_empty(), "org_member lacks {missing:?}");
    assert!(!held.holds(UserPermission::ManageOrgs));

    let again = register_token(&state, &clerk.token(&clerk.claims("new@example.test"))?).await?;
    assert_eq!(again.status(), StatusCode::CREATED);
    assert_eq!(
        json_str(&body_json(again).await?, "user_id")?,
        user_id.as_str()
    );
    assert_eq!(state.user_store.list_users().await?.len(), 1);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RefusedClerkToken {
    NotAJwt,
    NotBearer,
    Unsigned,
    AnotherInstancesKey,
    AnotherIssuer,
    Expired,
    NotYetValid,
    IssuedAhead,
    IssuedLongAgo,
    AnotherOrigin,
    AnotherTemplate,
    Impersonated,
    UnverifiedEmail,
    UnverifiedEmailAsAString,
    NoEmail,
    Replayed,
}

/// A token registers only if this instance signed it, for this deployment, from
/// the `pipette-mgmt` template, recently, and for a verified address; and only once.
#[rstest]
#[case::not_a_jwt(RefusedClerkToken::NotAJwt, "not a JWT")]
#[case::not_bearer(RefusedClerkToken::NotBearer, "missing Authorization: Bearer")]
#[case::unsigned(RefusedClerkToken::Unsigned, "not signed with RS256")]
#[case::another_instances_key(RefusedClerkToken::AnotherInstancesKey, "signature does not verify")]
#[case::another_issuer(RefusedClerkToken::AnotherIssuer, "another Clerk instance")]
#[case::expired(RefusedClerkToken::Expired, "has expired")]
#[case::not_yet_valid(RefusedClerkToken::NotYetValid, "not valid yet")]
#[case::issued_ahead(RefusedClerkToken::IssuedAhead, "not valid yet")]
#[case::issued_long_ago(RefusedClerkToken::IssuedLongAgo, "more than five minutes ago")]
#[case::another_origin(RefusedClerkToken::AnotherOrigin, "another origin")]
#[case::another_template(RefusedClerkToken::AnotherTemplate, "pipette-mgmt JWT template")]
#[case::impersonated(RefusedClerkToken::Impersonated, "impersonated")]
#[case::unverified_email(RefusedClerkToken::UnverifiedEmail, "no verified email")]
#[case::unverified_email_as_a_string(
    RefusedClerkToken::UnverifiedEmailAsAString,
    "no verified email"
)]
#[case::no_email(RefusedClerkToken::NoEmail, "no verified email")]
#[case::replayed(RefusedClerkToken::Replayed, "already used")]
#[tokio::test]
async fn test_a_refused_clerk_token_registers_nothing(
    #[case] refused: RefusedClerkToken,
    #[case] reason: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    seed_org(&state, "acme").await?;
    let now = chrono::Utc::now().timestamp();
    let mut claims = clerk.claims("new@example.test");
    let bearer = |token: String| vec![("Authorization", format!("Bearer {token}"))];

    let headers = match refused {
        RefusedClerkToken::NotAJwt => bearer("not-a-jwt".into()),
        RefusedClerkToken::NotBearer => {
            vec![("Authorization", format!("Basic {}", clerk.token(&claims)?))]
        }
        RefusedClerkToken::Unsigned => {
            bearer(clerk.token_with_header(&json!({ "alg": "none" }), &claims)?)
        }
        RefusedClerkToken::AnotherInstancesKey => bearer(ClerkInstance::new()?.token(&claims)?),
        RefusedClerkToken::AnotherIssuer => {
            claims["iss"] = json!("https://clerk.other.example.test");
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::Expired => {
            claims["iat"] = json!(now - 120);
            claims["nbf"] = json!(now - 130);
            claims["exp"] = json!(now - 60);
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::NotYetValid => {
            claims["iat"] = json!(now + 120);
            claims["nbf"] = json!(now + 110);
            claims["exp"] = json!(now + 180);
            bearer(clerk.token(&claims)?)
        }
        // A future iat is refused on its own, with nbf already reached.
        RefusedClerkToken::IssuedAhead => {
            claims["iat"] = json!(now + 120);
            claims["nbf"] = json!(now - 10);
            claims["exp"] = json!(now + 180);
            bearer(clerk.token(&claims)?)
        }
        // A template with a long lifetime still registers only while fresh.
        RefusedClerkToken::IssuedLongAgo => {
            claims["iat"] = json!(now - 600);
            claims["nbf"] = json!(now - 610);
            claims["exp"] = json!(now + 3600);
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::AnotherOrigin => {
            claims["azp"] = json!("https://other.example.test");
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::AnotherTemplate => {
            claims["aud"] = json!("another-app");
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::Impersonated => {
            claims["act"] = json!({ "sub": "user_2admin" });
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::UnverifiedEmail => {
            claims["email_verified"] = json!(false);
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::UnverifiedEmailAsAString => {
            claims["email_verified"] = json!("false");
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::NoEmail => {
            claims
                .as_object_mut()
                .context("claims are an object")?
                .remove("email");
            bearer(clerk.token(&claims)?)
        }
        RefusedClerkToken::Replayed => {
            let token = clerk.token(&claims)?;
            let first = register_token(&state, &token).await?;
            assert_eq!(
                first.status(),
                StatusCode::CREATED,
                "the first one registers"
            );
            bearer(token)
        }
    };

    let resp = register_with(&state, &headers).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let error = error_of(resp).await?;
    assert!(error.contains(reason), "{refused:?}: {error}");
    if !matches!(refused, RefusedClerkToken::Replayed) {
        assert!(
            state.user_store.list_users().await?.is_empty(),
            "a refused token provisions nothing"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RefusedClerkAccount {
    UnprovisionedDomain,
    PendingAccount,
    DisabledAccount,
    ServiceAccount,
    MissingOrganization,
    DisabledOrganization,
}

/// A verified address registers only to a person's account in good standing,
/// and is given one only under a rule naming an organization that is there to
/// join.
#[rstest]
#[case::unprovisioned_domain(
    RefusedClerkAccount::UnprovisionedDomain,
    StatusCode::FORBIDDEN,
    "no account holds person@elsewhere.example"
)]
#[case::pending_account(RefusedClerkAccount::PendingAccount, StatusCode::FORBIDDEN, "pending")]
#[case::disabled_account(
    RefusedClerkAccount::DisabledAccount,
    StatusCode::FORBIDDEN,
    "user is disabled"
)]
#[case::service_account(
    RefusedClerkAccount::ServiceAccount,
    StatusCode::FORBIDDEN,
    "a service account signs with its own keys"
)]
#[case::missing_organization(
    RefusedClerkAccount::MissingOrganization,
    StatusCode::INTERNAL_SERVER_ERROR,
    "internal server error"
)]
#[case::disabled_organization(
    RefusedClerkAccount::DisabledOrganization,
    StatusCode::FORBIDDEN,
    "acme, which is disabled"
)]
#[tokio::test]
async fn test_a_verified_address_without_a_usable_account_registers_nothing(
    #[case] refused: RefusedClerkAccount,
    #[case] status: StatusCode,
    #[case] reason: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    let mut email = "person@example.test";
    let mut accounts = 0;

    match refused {
        RefusedClerkAccount::UnprovisionedDomain => {
            seed_org(&state, "acme").await?;
            email = "person@elsewhere.example";
        }
        // At a domain with no rule: one with a rule refuses the keyless request.
        RefusedClerkAccount::PendingAccount => {
            seed_org(&state, "acme").await?;
            email = "person@elsewhere.example";
            let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
            let resp = request_user_account(&state, email, &key, "test laptop").await?;
            assert_eq!(resp.status(), StatusCode::CREATED);
            accounts = 1;
        }
        // Offboarded, and not handed a fresh account by the provision rule.
        RefusedClerkAccount::DisabledAccount => {
            seed_org(&state, "acme").await?;
            let user = create_user(&state, email).await?;
            set_user_status(&state, &user.user_id, UserStatus::Disabled).await?;
            accounts = 1;
        }
        // Its holder signs with the keys it registered, never as a person.
        RefusedClerkAccount::ServiceAccount => {
            seed_org(&state, "acme").await?;
            let user = create_user(&state, email).await?;
            state
                .user_store
                .put_user(&User {
                    account_type: UserAccountType::Service,
                    ..user.clone()
                })
                .await?;
            accounts = 1;
        }
        RefusedClerkAccount::MissingOrganization => {}
        RefusedClerkAccount::DisabledOrganization => {
            let mut acme = seed_org(&state, "acme").await?;
            acme.status = pipette_plan_types::OrgStatus::Disabled;
            state.org_store.put_org(&acme).await?;
        }
    }

    let resp = register_token(&state, &clerk.token(&clerk.claims(email))?).await?;
    assert_eq!(resp.status(), status);
    let error = error_of(resp).await?;
    assert!(error.contains(reason), "{refused:?}: {error}");
    assert_eq!(state.user_store.list_users().await?.len(), accounts);
    if matches!(refused, RefusedClerkAccount::ServiceAccount) {
        let holder = state
            .user_store
            .lookup_user_by_email(email)
            .await?
            .context("the service account holds its address")?;
        assert!(state.user_store.list_user_keys(&holder).await?.is_empty());
        assert!(
            state.user_store.user_awaits_sign_in(&holder).await?,
            "a refused sign-in is not its first"
        );
    }
    assert!(
        accounts > 0
            || state
                .user_store
                .lookup_user_by_email(email)
                .await?
                .is_none(),
        "a refusal leaves the address unclaimed"
    );
    Ok(())
}

/// A claim whose account record a create died before writing is a `409` naming
/// the holder: no key, and no account provisioned over the claim.
#[tokio::test]
async fn test_a_stranded_address_claim_registers_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    seed_org(&state, "acme").await?;
    let stranded =
        pipette_plan_types::UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
    state
        .user_store
        .claim_user_email("stranded@example.test", &stranded)
        .await?;

    let resp = register_token(
        &state,
        &clerk.token(&clerk.claims("stranded@example.test"))?,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        error_of(resp).await?,
        format!(
            "the address is claimed by {stranded}, which has no account record yet; \
             try again, or ask an operator to finish it"
        )
    );
    assert!(state.user_store.list_users().await?.is_empty());
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("stranded@example.test")
            .await?,
        Some(stranded),
        "the claim stands"
    );
    Ok(())
}

/// Another domain is refused before the account is looked up; the match is
/// exact.
#[rstest]
#[case::allowed("operator@Example.test", StatusCode::CREATED)]
#[case::outside("operator@elsewhere.example", StatusCode::FORBIDDEN)]
#[case::subdomain("operator@sub.example.test", StatusCode::FORBIDDEN)]
#[case::look_alike("operator@evilexample.test", StatusCode::FORBIDDEN)]
#[tokio::test]
async fn test_allowed_domains_limits_who_registers_through_clerk(
    #[case] email: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let mut section = clerk.config(&[])?;
    section.allowed_domains = vec!["@Example.test".to_string()];
    let state = make_state_with_clerk_section(dir.path(), section).await?;
    create_user(&state, &email.to_lowercase()).await?;

    let resp = register_token(&state, &clerk.token(&clerk.claims(email))?).await?;
    assert_eq!(resp.status(), status, "{email}");
    if status == StatusCode::FORBIDDEN {
        let error = error_of(resp).await?;
        assert!(
            error.contains("limited to addresses at example.test"),
            "{error}"
        );
    }
    Ok(())
}

/// Refused so no unverified claim holds the address against the Clerk sign-in
/// that would create it. Other domains still ask.
#[rstest]
#[case::provisioned("person@Example.test", StatusCode::FORBIDDEN)]
#[case::elsewhere("person@elsewhere.example", StatusCode::CREATED)]
#[tokio::test]
async fn test_a_provisioned_address_is_not_requested_without_clerk(
    #[case] email: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    seed_org(&state, "acme").await?;
    let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);

    let resp = request_user_account(&state, email, &key, "test laptop").await?;
    assert_eq!(resp.status(), status);
    if status == StatusCode::CREATED {
        return Ok(());
    }
    let error = error_of(resp).await?;
    assert!(error.contains("through Clerk"), "{error}");
    assert!(state.user_store.list_users().await?.is_empty());

    let registered = register_token(&state, &clerk.token(&clerk.claims(email))?).await?;
    assert_eq!(
        registered.status(),
        StatusCode::CREATED,
        "Clerk still creates it"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum BadClerkSection {
    NotAPublishableKey,
    NoAuthorizedParty,
    AnAuthorizedPartyWithAPath,
    NotAPemKey,
    EmptyJwtTemplate,
    BlankJwtTemplate,
    AnAddressForADomain,
    NotAnOrganizationHandle,
    OneDomainTwice,
    NotAnAllowedDomain,
    ProvisionOutsideAllowedDomains,
}

/// A `[clerk]` section the server cannot use stops it at startup, rather than
/// refusing every sign-in later.
#[rstest]
#[case::not_a_publishable_key(BadClerkSection::NotAPublishableKey)]
#[case::no_authorized_party(BadClerkSection::NoAuthorizedParty)]
#[case::an_authorized_party_with_a_path(BadClerkSection::AnAuthorizedPartyWithAPath)]
#[case::not_a_pem_key(BadClerkSection::NotAPemKey)]
#[case::empty_jwt_template(BadClerkSection::EmptyJwtTemplate)]
#[case::blank_jwt_template(BadClerkSection::BlankJwtTemplate)]
#[case::an_address_for_a_domain(BadClerkSection::AnAddressForADomain)]
#[case::not_an_organization_handle(BadClerkSection::NotAnOrganizationHandle)]
#[case::one_domain_twice(BadClerkSection::OneDomainTwice)]
#[case::not_an_allowed_domain(BadClerkSection::NotAnAllowedDomain)]
// It would create accounts that cannot sign in.
#[case::provision_outside_allowed_domains(BadClerkSection::ProvisionOutsideAllowedDomains)]
#[tokio::test]
async fn test_a_bad_clerk_section_stops_startup(
    #[case] bad: BadClerkSection,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut section = ClerkInstance::new()?.config(&[("example.test", "acme")])?;
    match bad {
        // `clerk.example.test` without the `$` a publishable key ends its host with.
        BadClerkSection::NotAPublishableKey => {
            section.publishable_key = "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0".into();
        }
        BadClerkSection::NoAuthorizedParty => section.authorized_parties.clear(),
        BadClerkSection::AnAuthorizedPartyWithAPath => {
            section.authorized_parties = vec!["https://console.example.test/app".into()];
        }
        BadClerkSection::NotAPemKey => {
            section.public_key =
                "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----".into();
        }
        BadClerkSection::EmptyJwtTemplate => section.jwt_template = String::new(),
        BadClerkSection::BlankJwtTemplate => section.jwt_template = " \t".into(),
        BadClerkSection::AnAddressForADomain => {
            section.provision[0].domain = "ops@example.test".into()
        }
        BadClerkSection::NotAnOrganizationHandle => {
            section.provision[0].org = "Not A Handle!".into()
        }
        BadClerkSection::OneDomainTwice => {
            let again = section.provision[0].clone();
            section.provision.push(again);
        }
        BadClerkSection::NotAnAllowedDomain => {
            section.allowed_domains = vec!["example.test/app".into()];
        }
        BadClerkSection::ProvisionOutsideAllowedDomains => {
            section.allowed_domains = vec!["elsewhere.example".into()];
        }
    }
    let Err(e) = make_state_with_clerk_section(dir.path(), section).await else {
        anyhow::bail!("{bad:?} started");
    };
    assert!(format!("{e:#}").contains("[clerk]"), "{bad:?}: {e:#}");
    Ok(())
}

/// A state trusting `clerk` whose audit log is the returned capture.
async fn audited_state(
    dir: &std::path::Path,
    clerk: &ClerkInstance,
    provision: &[(&str, &str)],
) -> anyhow::Result<(AppState, Arc<CapturingAuditStore>)> {
    setup_benchmarks(dir)?;
    let mut config = test_config(dir);
    config.clerk = Some(clerk.config(provision)?);
    let log = Arc::new(CapturingAuditStore::default());
    let stores = Stores {
        audit: Some(log.clone()),
        ..build_local_fs_stores(&config)?
    };
    let state = AppState::new(config, stores, test_http_client()?).await?;
    Ok((state, log))
}

/// Sign in as `email` with a fresh browser key: the account, and the key that
/// signs as it.
async fn sign_in(
    state: &AppState,
    clerk: &ClerkInstance,
    email: &str,
) -> anyhow::Result<(UserId, ed25519_dalek::SigningKey, UserKeyId)> {
    let browser = signing_key()?;
    let body = json!({
        "public_key": hex::encode(browser.verifying_key().as_bytes()),
        "key_details": "console app, a browser",
    });
    let resp = register(state, &clerk.token(&clerk.claims(email))?, &body).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let receipt = body_json(resp).await?;
    Ok((
        UserId::try_new(json_str(&receipt, "user_id")?)?,
        browser,
        UserKeyId::try_new(json_str(&receipt, "key_id")?)?,
    ))
}

/// The first Clerk sign-in records the keys the account held before it, shows
/// them to the owner and logs them when there are any. Nothing is revoked, and
/// a later sign-in changes neither the record nor the log.
#[rstest]
#[case::an_account_holding_a_key(true, true)]
#[case::an_account_holding_none(true, false)]
#[case::an_account_the_sign_in_creates(false, false)]
#[tokio::test]
async fn test_the_first_sign_in_surfaces_the_keys_held_before_it(
    #[case] existing: bool,
    #[case] holds_a_key: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let (state, log) = audited_state(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    seed_org(&state, "acme").await?;
    let email = "operator@example.test";
    let account = if existing {
        Some(create_user(&state, email).await?)
    } else {
        None
    };
    // As an administrator's pre-auth key would register one.
    let planted = match (&account, holds_a_key) {
        (Some(user), true) => {
            let public_key = hex::encode(signing_key()?.verifying_key().as_bytes());
            let key = UserKey::new(
                PublicKeyHex::try_new(public_key)?,
                NonEmptyTrimmedString::try_new("an administrator's laptop")?,
                chrono::Utc::now(),
            );
            state.user_store.put_user_key(&user.user_id, &key).await?;
            vec![key.key_id]
        }
        _ => Vec::new(),
    };
    let logged = || -> Vec<(AuditActor, Vec<UserKeyId>, BTreeSet<AuditTarget>)> {
        log.events()
            .into_iter()
            .filter_map(|event| match event.action {
                AuditAction::FirstClerkSignIn { prior_keys } => Some((
                    event.actor,
                    prior_keys.into_iter().map(|key| key.key_id).collect(),
                    event.targets,
                )),
                AuditAction::Http { .. } | AuditAction::Cli { .. } => None,
            })
            .collect()
    };

    let (user_id, browser, key_id) = sign_in(&state, &clerk, email).await?;
    let first = state
        .user_store
        .get_user_first_clerk_sign_in(&user_id)
        .await?
        .context("the first sign-in is recorded")?;
    assert_eq!(
        first.prior_keys, planted,
        "the browser's own key is not prior"
    );
    let resp = authed_user_get(&state, &browser, &user_id, &key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let me = body_json(resp).await?;
    assert_eq!(
        me["first_clerk_sign_in"]["prior_keys"],
        json!(planted.iter().map(UserKeyId::as_str).collect::<Vec<_>>())
    );
    assert_eq!(
        me["keys"].as_array().map(Vec::len),
        Some(planted.len() + 1),
        "nothing is revoked"
    );
    let event = (
        AuditActor::User {
            user_id: user_id.clone(),
            key_id,
        },
        planted.clone(),
        BTreeSet::from([AuditTarget::User {
            user_id: user_id.clone(),
        }]),
    );
    let expected = if planted.is_empty() {
        Vec::new()
    } else {
        vec![event]
    };
    assert_eq!(logged(), expected);

    sign_in(&state, &clerk, email).await?;
    assert_eq!(
        state
            .user_store
            .get_user_first_clerk_sign_in(&user_id)
            .await?,
        Some(first),
        "a later sign-in records nothing"
    );
    assert_eq!(logged(), expected, "a later sign-in logs nothing");
    Ok(())
}

/// The record is best effort: a store that cannot keep it still answers the
/// sign-in with its key, and the next sign-in is the first.
#[tokio::test]
async fn test_a_failed_first_sign_in_record_still_signs_in() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let (state, _log) = audited_state(dir.path(), &clerk, &[("example.test", "acme")]).await?;
    seed_org(&state, "acme").await?;
    let email = "operator@example.test";
    // A file where the records' directory goes fails every read and write of one.
    let blocker = dir.path().join("user-first-clerk-sign-in");
    std::fs::write(&blocker, b"")?;

    let (user_id, ..) = sign_in(&state, &clerk, email).await?;
    std::fs::remove_file(&blocker)?;
    let users = &*state.user_store;
    assert_eq!(users.get_user_first_clerk_sign_in(&user_id).await?, None);
    sign_in(&state, &clerk, email).await?;
    assert!(
        users
            .get_user_first_clerk_sign_in(&user_id)
            .await?
            .is_some()
    );
    Ok(())
}
