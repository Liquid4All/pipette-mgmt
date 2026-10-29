//! A service account works for one organization and administers nothing: it
//! holds no admin name and is refused while it belongs to no organization. The
//! administration routes that refuse it outright are `route_table`'s.

use anyhow::Context;
use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{CampaignId, OrgId, PrincipalOwner, UserAccountType, UserPermission};

use pipette_web_testkit::*;

async fn state() -> anyhow::Result<(tempfile::TempDir, AppState)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    Ok((dir, state))
}

/// Written to the store directly, as a grant made before the rule would be.
async fn service_account(
    state: &AppState,
    org: Option<&OrgId>,
    permissions: &[UserPermission],
) -> anyhow::Result<UserSigner> {
    let signer = registered_user(state, permissions).await?;
    make_service_account(state, &signer.1, org.cloned()).await?;
    Ok(signer)
}

async fn server_admin(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, UserPermissionGroup::ServerAdmin.permissions()).await
}

async fn error_of(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    Ok(json_str(&body_json(resp).await?, "error")?.to_string())
}

#[rstest]
#[case::create("POST", "/api/v1/users", json!({
    "contact_email": "ops+ci@example.test",
    "account_type": "service",
    "permissions": ["submit_plans", "manage_orgs"],
}))]
#[case::amend(
    "PATCH",
    "/api/v1/users/{id}/permissions",
    json!({ "add": ["submit_plans", "manage_orgs"] })
)]
#[case::set(
    "PUT",
    "/api/v1/users/{id}/permissions",
    json!({ "permissions": ["submit_plans", "manage_orgs"] })
)]
#[tokio::test]
async fn a_service_account_is_granted_no_admin_name(
    #[case] method: &str,
    #[case] path: &str,
    #[case] body: Value,
) -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let admin = server_admin(&state).await?;
    let org = seed_org(&state, "acme").await?;
    let service = service_account(&state, Some(&org.org_id), &[]).await?;
    let uri = path.replace("{id}", service.1.as_str());

    let resp = signed_json(&state, &admin, method, &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        error_of(resp).await?,
        "a service account cannot hold manage_orgs"
    );
    assert!(
        state
            .user_store
            .get_user_permissions(&service.1)
            .await?
            .is_none(),
        "nothing was written"
    );
    Ok(())
}

/// Not created, released or retyped into none: it would be refused on every
/// route.
#[rstest]
#[case::create("POST", "/api/v1/users", json!({
    "contact_email": "ops+ci@example.test",
    "account_type": "service",
}))]
#[case::release("PUT", "/api/v1/users/{service}/org", json!({ "org_id": null }))]
#[case::retype("PATCH", "/api/v1/users/{loner}", json!({ "account_type": "service" }))]
#[tokio::test]
async fn a_service_account_is_never_left_in_no_organization(
    #[case] method: &str,
    #[case] path: &str,
    #[case] body: Value,
) -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let admin = server_admin(&state).await?;
    let org = seed_org(&state, "acme").await?;
    let service = service_account(&state, Some(&org.org_id), &[]).await?;
    let loner = create_user(&state, "loner@example.test").await?;
    let uri = path
        .replace("{service}", service.1.as_str())
        .replace("{loner}", loner.user_id.as_str());

    let resp = signed_json(&state, &admin, method, &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(error_of(resp).await?.contains("belongs to an organization"));
    assert!(
        state
            .user_store
            .lookup_user_by_email("ops+ci@example.test")
            .await?
            .is_none(),
        "nothing was created"
    );
    assert_eq!(
        state.org_store.get_membership(&service.1).await?,
        Some(org.org_id),
        "the service account stays"
    );
    assert_eq!(
        state.user_store.get_user(&loner.user_id).await?,
        Some(loner),
        "the account keeps its type"
    );
    Ok(())
}

#[tokio::test]
async fn an_admin_is_not_retyped_as_a_service_account() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let admin = server_admin(&state).await?;
    let org = seed_org(&state, "acme").await?;
    let user = create_user(&state, "ops@example.test").await?;
    set_user_org(&state, &user.user_id, Some(org.org_id)).await?;
    grant_user(&state, &user.user_id, &[UserPermission::ManageOrgs]).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &format!("/api/v1/users/{}", user.user_id),
        &json!({ "account_type": "service" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(error_of(resp).await?.contains("holds manage_orgs"));
    Ok(())
}

/// An org admin lacking the name can neither mint the account a key nor retype
/// it a person's, which would make the name count again.
#[rstest]
#[case::mint("POST", "/api/v1/users/{id}/preauth-keys", json!({}))]
#[case::retype("PATCH", "/api/v1/users/{id}", json!({ "account_type": "human" }))]
#[tokio::test]
async fn a_stored_admin_name_still_outranks_an_org_admin(
    #[case] method: &str,
    #[case] path: &str,
    #[case] body: Value,
) -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let org = seed_org(&state, "acme").await?;
    let service =
        service_account(&state, Some(&org.org_id), &[UserPermission::ManageAnyOrgs]).await?;
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &admin.1, Some(org.org_id)).await?;

    let uri = path.replace("{id}", service.1.as_str());
    let resp = signed_json(&state, &admin, method, &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(error_of(resp).await?.contains("manage_any_orgs"));
    let user = state
        .user_store
        .get_user(&service.1)
        .await?
        .context("the account exists")?;
    assert_eq!(user.account_type, UserAccountType::Service);
    Ok(())
}

#[tokio::test]
async fn an_admin_name_a_service_account_already_holds_grants_nothing() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let org = seed_org(&state, "acme").await?;
    let service = service_account(
        &state,
        Some(&org.org_id),
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;

    let me = body_json(signed_empty(&state, &service, "GET", "/api/v1/users/me").await?).await?;
    let held: Vec<&str> = me
        .pointer("/permissions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(held, ["read_plans", "submit_plans"]);

    // `read_orgs` comes only through `manage_orgs`, which does not count.
    let resp = signed_empty(&state, &service, "GET", "/api/v1/users").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// The refusal does not ask an administrator to grant the name: here
/// `cancel_any_plans`.
#[tokio::test]
async fn a_refusal_names_what_a_service_account_cannot_hold() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let org = seed_org(&state, "acme").await?;
    let colleague = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (_, plan_id) = created_campaign(&state, &colleague, &[("name", "Nightly")]).await?;
    let service =
        service_account(&state, Some(&org.org_id), &[UserPermission::CancelPlans]).await?;

    let uri = format!("/api/v1/plans/{plan_id}/cancel");
    let resp = signed_empty(&state, &service, "POST", &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        error_of(resp).await?,
        "a service account cannot hold the cancel_any_plans permission"
    );
    Ok(())
}

/// Refused before any handler runs; its own record stays readable.
#[tokio::test]
async fn a_service_account_on_no_organization_is_refused() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let service = service_account(&state, None, &[UserPermission::SubmitPlans]).await?;

    let resp = signed_empty(&state, &service, "GET", "/api/v1/vault/resources").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        error_of(resp).await?,
        "a service account must belong to an organization"
    );
    assert_eq!(
        signed_empty(&state, &service, "GET", "/api/v1/users/me")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

/// A service account files into its organization's campaign and cancels its
/// plans as any member does, and renames no record it does not own.
#[tokio::test]
async fn a_service_account_files_into_its_organization_s_campaign() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let org = seed_org(&state, "acme").await?;
    let service = service_account(
        &state,
        Some(&org.org_id),
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let author = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &author.1, Some(org.org_id.clone())).await?;
    let params = [("name", "Team"), ("owner", org.org_id.as_str())];
    let (campaign_id, first) = created_campaign(&state, &author, &params).await?;

    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let resp = post_plan_document(&state, &service, &format!("{uri}/plans"), ONE_CELL_PLAN).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let cancel = format!("/api/v1/plans/{first}/cancel");
    let resp = signed_empty(&state, &service, "POST", &cancel).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let resp = signed_json(&state, &service, "PATCH", &uri, &json!({ "name": "Mine" })).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_service_account_administers_the_campaign_it_creates() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let org = seed_org(&state, "acme").await?;
    let service =
        service_account(&state, Some(&org.org_id), &[UserPermission::SubmitPlans]).await?;

    let (campaign_id, _) = created_campaign(&state, &service, &[("name", "Nightly")]).await?;
    let campaign = state
        .campaign_store
        .get_campaign(&CampaignId::try_new(campaign_id.as_str())?)
        .await?
        .context("the campaign was created")?;
    assert_eq!(
        campaign.owner,
        PrincipalOwner::User {
            user_id: service.1.clone()
        }
    );

    let resp = signed_json(
        &state,
        &service,
        "PATCH",
        &format!("/api/v1/campaigns/{campaign_id}"),
        &json!({ "name": "Nightly, renamed" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_service_account_deletes_what_it_uploads() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let org = seed_org(&state, "acme").await?;
    let service = service_account(&state, Some(&org.org_id), &[]).await?;

    let sha256 = upload_vault_blob(&state, &service, service.1.as_str(), VAULT_GGUF).await?;
    let resp = describe_vault_upload(&state, &service, &gguf_text_upload(None, &sha256)).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let answer = body_json(resp).await?;
    assert_eq!(
        answer.pointer("/resources/0/owner"),
        Some(&json!({ "type": "user", "user_id": service.1 }))
    );
    let resource_id = answer
        .pointer("/resources/0/resource_id")
        .and_then(Value::as_str)
        .context("the answer names the resource")?;

    let uri = format!("/api/v1/vault/resources/{resource_id}");
    let resp = signed_empty(&state, &service, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}
