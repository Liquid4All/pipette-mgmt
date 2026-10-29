//! The device pre-auth keys an organization's administrator mints, end to end:
//! `GET`, `POST` and `DELETE` under `/api/v1/orgs/{org}/client-preauth-keys`,
//! and the registration that presents one.

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::client::ClientStatus;
use pipette_core::client_preauth::ClientPreauthParams;
use pipette_core::types::DeriveFromKey;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    ClientId, ClientOwner, ClientPermission, ClientPreauthUsage, OrgId, PublicKeyHex, Tag,
    UserPermission, UserStatus,
};
use pipette_web_testkit::*;

const ORG_ADMIN: &[UserPermission] = &[UserPermission::ManageClients, UserPermission::ReadClients];

async fn org_with_admin(state: &AppState, slug: &str) -> anyhow::Result<(OrgId, UserSigner)> {
    let org = seed_org(state, slug).await?;
    let admin = registered_user(state, ORG_ADMIN).await?;
    set_user_org(state, &admin.1, Some(org.org_id.clone())).await?;
    Ok((org.org_id, admin))
}

fn keys_uri(org: &impl std::fmt::Display) -> String {
    format!("/api/v1/orgs/{org}/client-preauth-keys")
}

async fn mint_token(
    state: &AppState,
    signer: &UserSigner,
    org: &OrgId,
    body: &Value,
) -> anyhow::Result<String> {
    let resp = signed_json(state, signer, "POST", &keys_uri(org), body).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::CREATED,
        "mint failed with {}",
        resp.status()
    );
    Ok(json_str(&body_json(resp).await?, "token")?.to_string())
}

/// A new keypair presenting `token` at registration: the raw response, since
/// half the callers expect a refusal.
async fn present(
    state: &AppState,
    token: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    present_as(state, token, &signing_key()?).await
}

/// [`present`] with the caller's keypair, to look for what a refusal left.
async fn present_as(
    state: &AppState,
    token: &str,
    key: &ed25519_dalek::SigningKey,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    unauthed_post(
        state,
        "/clients/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "organization": "a label, not the owner",
            "client_details": "rack 3",
            "contact_email": "lab@example.test",
            "client_preauth_key": token,
        }),
    )
    .await
}

async fn register(state: &AppState, token: &str) -> anyhow::Result<ClientId> {
    let resp = present(state, token).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::CREATED,
        "registration failed with {}",
        resp.status()
    );
    let body = body_json(resp).await?;
    anyhow::ensure!(json_str(&body, "status")? == "approved", "{body}");
    Ok(ClientId::try_new(json_str(&body, "client_id")?)?)
}

async fn listed(state: &AppState, signer: &UserSigner, org: &OrgId) -> anyhow::Result<Vec<Value>> {
    let resp = signed_empty(state, signer, "GET", &keys_uri(org)).await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("ClientPreauthKeyViewPage", &body)?;
    let rows = body
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("a listing carries items: {body}"))?;
    anyhow::ensure!(
        rows.iter()
            .all(|row| row.get("org_id").and_then(Value::as_str) == Some(org.as_str())),
        "{body}"
    );
    Ok(rows)
}

/// The device arrives approved and the organization's, with no server admin.
#[tokio::test]
async fn an_org_admin_onboards_a_device_with_a_preauth_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;

    let request = json!({ "expires_in_days": 7, "note": "rack 3" });
    crate::api_schema::assert_conforms("MintClientPreauthRequest", &request)?;
    let resp = signed_json(&state, &admin, "POST", &keys_uri(&acme), &request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "a credential is not cached"
    );
    let minted = body_json(resp).await?;
    crate::api_schema::assert_conforms("ClientPreauthKeyMinted", &minted)?;
    let token = json_str(&minted, "token")?.to_string();
    let secret = token
        .split_once('.')
        .map(|(_, secret)| secret.to_string())
        .ok_or_else(|| anyhow::anyhow!("a token carries its secret after the dot"))?;
    assert!(token.starts_with("client_preauth_"), "{token}");
    assert_eq!(minted["usage"], json!("single_use"));
    assert_eq!(minted["org_id"], json!(acme.as_str()));
    assert_eq!(minted["minted_by"], json!(admin.1.as_str()));
    assert_eq!(minted["note"], json!("rack 3"));
    assert!(minted.get("secret_hash").is_none(), "{minted}");
    let expires_at: chrono::DateTime<chrono::Utc> = json_str(&minted, "expires_at")?.parse()?;
    let hours = (expires_at - chrono::Utc::now()).num_hours();
    assert!((6 * 24..=7 * 24).contains(&hours), "expires in {hours}h");

    // Listed by its id and its minter, never by its secret.
    let keys = listed(&state, &admin, &acme).await?;
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["key_id"], minted["key_id"]);
    assert_eq!(keys[0]["minted_by"], json!(admin.1.as_str()));
    let text = Value::Array(keys).to_string();
    assert!(
        !text.contains(&secret) && !text.contains("secret_hash"),
        "{text}"
    );

    // The device arrives approved, the organization's, and holding what any
    // registration holds: the key seeds the owner and nothing else.
    let client_id = register(&state, &token).await?;
    let client = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the device was registered"))?;
    assert_eq!(client.status, ClientStatus::Approved);
    assert_eq!(client.organization.as_str(), "a label, not the owner");
    assert_eq!(
        state.org_store.get_ownership(&client_id).await?,
        Some(ClientOwner::Org {
            org_id: acme.clone()
        })
    );
    let permissions = state
        .client_store
        .get_client_permissions(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registration writes the set"))?;
    assert_eq!(permissions.names(), ["submit_results"]);
    assert!(
        state
            .tag_store
            .get_client_tags(&client_id)
            .await?
            .is_empty()
    );

    // Spent: gone from the listing, and it registers nothing more.
    assert!(listed(&state, &admin, &acme).await?.is_empty());
    assert_eq!(
        present(&state, &token).await?.status(),
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

/// A multi-use key brings up a rack, and a revoke stops it. A key is revoked
/// through the organization it names, so a repeat finds nothing.
#[tokio::test]
async fn a_multi_use_key_registers_devices_until_it_is_revoked() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;

    let token = mint_token(&state, &admin, &acme, &json!({ "usage": "multi_use" })).await?;
    let first = register(&state, &token).await?;
    let second = register(&state, &token).await?;
    assert_ne!(first, second);

    let keys = listed(&state, &admin, &acme).await?;
    assert_eq!(keys.len(), 1, "a multi-use key is never spent");
    assert_eq!(keys[0]["usage"], json!("multi_use"));
    let uri = format!("{}/{}", keys_uri(&acme), json_str(&keys[0], "key_id")?);

    let resp = signed_empty(&state, &admin, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        present(&state, &token).await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let resp = signed_empty(&state, &admin, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A mint reads `usage`, single use when absent.
#[rstest]
#[case::multi_use(json!({ "usage": "multi_use" }), "multi_use")]
#[case::single_use(json!({ "usage": "single_use" }), "single_use")]
#[case::absent(json!({}), "single_use")]
#[tokio::test]
async fn a_mint_reads_its_usage(#[case] request: Value, #[case] usage: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;

    let resp = signed_json(&state, &admin, "POST", &keys_uri(&acme), &request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED, "{request}");
    assert_eq!(body_json(resp).await?["usage"], json!(usage), "{request}");
    Ok(())
}

/// The scope is where an assignment may put a device down: the caller's own
/// organization. Any other reads as missing, so the path cannot probe which
/// organizations exist, and a key is not revoked through another organization's path.
#[tokio::test]
async fn a_key_is_minted_for_the_callers_own_organization_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;
    let (rival, rival_admin) = org_with_admin(&state, "rival").await?;
    let token = mint_token(
        &state,
        &rival_admin,
        &rival,
        &json!({ "usage": "multi_use" }),
    )
    .await?;
    let rival_key = listed(&state, &rival_admin, &rival).await?[0]["key_id"].clone();
    let rival_key = rival_key
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("a key id"))?;

    // A read of the fleet is not the grant, and neither is the roster's.
    let reader = registered_user(
        &state,
        &[
            UserPermission::ReadAnyClients,
            UserPermission::ManageAnyOrgs,
        ],
    )
    .await?;
    // The grant with no membership administers nothing.
    let unaffiliated = registered_user(&state, ORG_ADMIN).await?;

    for (signer, expected, who) in [
        (&reader, StatusCode::FORBIDDEN, "without manage_clients"),
        (
            &admin,
            StatusCode::NOT_FOUND,
            "another organization's admin",
        ),
        (
            &unaffiliated,
            StatusCode::NOT_FOUND,
            "an unaffiliated admin",
        ),
    ] {
        let uri = keys_uri(&rival);
        let resp = signed_json(&state, signer, "POST", &uri, &json!({})).await?;
        assert_eq!(resp.status(), expected, "POST {who}");
        let resp = signed_empty(&state, signer, "GET", &uri).await?;
        assert_eq!(resp.status(), expected, "GET {who}");
        let resp = signed_empty(&state, signer, "DELETE", &format!("{uri}/{rival_key}")).await?;
        assert_eq!(resp.status(), expected, "DELETE {who}");
    }
    // Nor through its own path: the key names rival.
    let uri = format!("{}/{rival_key}", keys_uri(&acme));
    let resp = signed_empty(&state, &admin, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // An organization that does not exist answers what one out of reach does.
    let nosuch = "org_0195c3e2-0000-7000-8000-000000000000";
    let resp = signed_json(&state, &admin, "POST", &keys_uri(&nosuch), &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    assert_eq!(listed(&state, &rival_admin, &rival).await?.len(), 1);
    register(&state, &token).await?;

    // The fleet grant reaches every organization.
    let fleet = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;
    let token = mint_token(&state, &fleet, &rival, &json!({})).await?;
    let client_id = register(&state, &token).await?;
    assert_eq!(
        state.org_store.get_ownership(&client_id).await?,
        Some(ClientOwner::Org { org_id: rival })
    );
    Ok(())
}

/// A body asking for what a key minted here cannot carry is refused, rather
/// than answered with a key that lacks it.
#[rstest]
#[case::tags(json!({ "tags": ["lab"] }))]
#[case::permissions(json!({ "permissions": ["perform_work"] }))]
#[case::another_owner(json!({ "org_id": "org_0195c3e2-0000-7000-8000-000000000000" }))]
#[case::no_expiry(json!({ "expires_in_days": 0 }))]
#[case::past_the_ceiling(json!({ "expires_in_days": 91 }))]
#[case::a_long_note(json!({ "note": "n".repeat(4097) }))]
#[case::unknown_usage(json!({ "usage": "twice" }))]
#[case::usage_as_bool(json!({ "usage": true }))]
#[case::former_usage_name(json!({ "multi_use": true }))]
#[tokio::test]
async fn a_mint_refuses_what_it_cannot_carry(#[case] body: Value) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;

    let resp = signed_json(&state, &admin, "POST", &keys_uri(&acme), &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    assert!(
        state
            .client_store
            .list_client_preauth_keys()
            .await?
            .is_empty()
    );
    Ok(())
}

/// How a minter stops being able to mint the key it handed out.
#[derive(Clone, Copy, Debug)]
enum Lapse {
    LeftTheOrganization,
    LostTheGrant,
    Disabled,
}

/// So a copy an administrator kept registers nothing after they leave. A
/// refused single-use key is not spent: it stays for its admin to revoke.
#[rstest]
#[case::left(Lapse::LeftTheOrganization)]
#[case::lost_the_grant(Lapse::LostTheGrant)]
#[case::disabled(Lapse::Disabled)]
#[tokio::test]
async fn a_key_admits_nothing_once_its_minter_could_not_mint_it(
    #[case] lapse: Lapse,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;
    let token = mint_token(&state, &admin, &acme, &json!({ "usage": "multi_use" })).await?;
    register(&state, &token).await?;
    // Only a single-use key is consumed, so only it can show a refusal spent nothing.
    let single_use = mint_token(&state, &admin, &acme, &json!({})).await?;

    match lapse {
        Lapse::LeftTheOrganization => set_user_org(&state, &admin.1, None).await?,
        Lapse::LostTheGrant => {
            grant_user(&state, &admin.1, &[UserPermission::ReadClients]).await?;
        }
        Lapse::Disabled => set_user_status(&state, &admin.1, UserStatus::Disabled).await?,
    }
    for token in [&token, &single_use] {
        let key = signing_key()?;
        let resp = present_as(&state, token, &key).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{lapse:?}");
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains("no longer admits a device"), "{error}");
        let public_key = PublicKeyHex::try_new(hex::encode(key.verifying_key().as_bytes()))?;
        let refused = ClientId::derive(&public_key);
        assert!(state.client_store.get_client(&refused).await?.is_none());
    }
    assert_eq!(
        state.client_store.list_client_preauth_keys().await?.len(),
        2
    );
    assert!(
        state
            .client_store
            .list_spent_client_preauth_markers()
            .await?
            .is_empty()
    );
    Ok(())
}

/// The listing is every key that registers a device into the organization, the
/// host's among them, with what each seeds. A key naming another organization,
/// a member, or nobody is not on it.
#[tokio::test]
async fn the_listing_is_the_keys_naming_the_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (acme, admin) = org_with_admin(&state, "acme").await?;
    let rival = seed_org(&state, "rival").await?;

    let lab = Tag::try_new("lab")?;
    let host_key = |owner: Option<ClientOwner>| ClientPreauthParams {
        usage: ClientPreauthUsage::MultiUse,
        expires_at: None,
        default_tags: [lab.clone()].into(),
        default_organization: None,
        default_owner: owner,
        default_permissions: Some([ClientPermission::PerformWork].into()),
        note: None,
    };
    let mut ours = None;
    for owner in [
        Some(ClientOwner::Org {
            org_id: acme.clone(),
        }),
        Some(ClientOwner::Org {
            org_id: rival.org_id.clone(),
        }),
        Some(ClientOwner::User {
            user_id: admin.1.clone(),
        }),
        None,
    ] {
        let minted = pipette_core::client_preauth::mint(host_key(owner), chrono::Utc::now())?;
        state
            .client_store
            .put_client_preauth_key(&minted.key)
            .await?;
        ours.get_or_insert(minted);
    }
    let ours = ours.ok_or_else(|| anyhow::anyhow!("the first key names acme"))?;

    let keys = listed(&state, &admin, &acme).await?;
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0]["key_id"], json!(ours.key.key_id.as_str()));
    assert_eq!(keys[0]["default_tags"], json!(["lab"]));
    assert_eq!(keys[0]["default_permissions"], json!(["perform_work"]));
    assert_eq!(keys[0]["expires_at"], Value::Null);
    assert!(keys[0].get("minted_by").is_none(), "the host minted it");

    // The host's key carries no minter, so nothing is checked again: it
    // registers whoever administers the organization now.
    set_user_org(&state, &admin.1, None).await?;
    register(&state, ours.token.expose()).await?;
    Ok(())
}
