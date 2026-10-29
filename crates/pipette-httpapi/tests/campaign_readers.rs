//! A campaign reader is a service account that reads the campaigns its
//! `campaign_scope` names and their plans, and nothing else: not its
//! organization's campaigns, not a route that scopes itself, and no name beside
//! `read_plans`. The scope is replaced, never removed.

use anyhow::Context;
use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{OrgId, UserPermission};

use pipette_web_testkit::*;

async fn state() -> anyhow::Result<(tempfile::TempDir, AppState)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    Ok((dir, state))
}

async fn error_of(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    Ok(json_str(&body_json(resp).await?, "error")?.to_string())
}

/// The ids of a listing's items under `field`.
async fn listed(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    field: &str,
) -> anyhow::Result<Vec<String>> {
    let resp = signed_empty(state, signer, "GET", uri).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let page = body_json(resp).await?;
    Ok(page
        .pointer("/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(field).and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

/// An organization with two team campaigns, one to publish and one to keep,
/// and a service account in it holding `read_plans`.
struct Fixture {
    org: OrgId,
    admin: UserSigner,
    author: UserSigner,
    reader: UserSigner,
    published: (String, String),
    kept: (String, String),
}

async fn fixture(state: &AppState) -> anyhow::Result<Fixture> {
    let org = seed_org(state, "acme").await?.org_id;
    let admin = registered_user(state, UserPermissionGroup::OrgAdmin.permissions()).await?;
    set_user_org(state, &admin.1, Some(org.clone())).await?;
    let author = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(state, &author.1, Some(org.clone())).await?;
    let (published, published_plan) = created_campaign(
        state,
        &author,
        &[("name", "Published"), ("owner", org.as_str())],
    )
    .await?;
    let (kept, kept_plan) =
        created_campaign(state, &author, &[("name", "Kept"), ("owner", org.as_str())]).await?;
    let reader = registered_user(state, &[UserPermission::ReadPlans]).await?;
    make_service_account(state, &reader.1, Some(org.clone())).await?;
    Ok(Fixture {
        org,
        admin,
        author,
        reader,
        published: (published, published_plan.to_string()),
        kept: (kept, kept_plan.to_string()),
    })
}

async fn scope(
    state: &AppState,
    admin: &UserSigner,
    reader: &UserSigner,
    campaigns: &[&str],
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        admin,
        "PATCH",
        &format!("/api/v1/users/{}", reader.1),
        &json!({ "campaign_scope": campaigns }),
    )
    .await
}

#[tokio::test]
async fn a_campaign_reader_reads_its_listed_campaigns_alone() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let (published, published_plan) = &f.published;
    let (kept, kept_plan) = &f.kept;

    // A member of the organization, it reads the team's campaigns until scoped.
    let mut before = listed(&state, &f.reader, "/api/v1/campaigns", "campaign_id").await?;
    before.sort();
    let mut both = vec![published.clone(), kept.clone()];
    both.sort();
    assert_eq!(before, both);

    let resp = scope(&state, &f.admin, &f.reader, &[published]).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let answer = body_json(resp).await?;
    assert_eq!(answer.pointer("/campaign_scope"), Some(&json!([published])));
    assert_eq!(answer.pointer("/changed"), Some(&json!(true)));

    let campaigns = listed(&state, &f.reader, "/api/v1/campaigns", "campaign_id").await?;
    assert_eq!(campaigns, [published.as_str()]);
    for uri in ["/api/v1/plans", "/api/v1/plans?scope=mine"] {
        assert_eq!(
            listed(&state, &f.reader, uri, "plan_id").await?,
            [published_plan.as_str()],
            "{uri}"
        );
    }

    let resp = signed_empty(
        &state,
        &f.reader,
        "GET",
        &format!("/api/v1/campaigns/{published}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = signed_empty(
        &state,
        &f.reader,
        "GET",
        &format!("/api/v1/campaigns/{kept}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        error_of(resp).await?,
        "this campaign is not one this account reads"
    );
    let resp = signed_empty(
        &state,
        &f.reader,
        "GET",
        &format!("/api/v1/plans/{published_plan}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = signed_empty(
        &state,
        &f.reader,
        "GET",
        &format!("/api/v1/plans/{kept_plan}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let me = body_json(signed_empty(&state, &f.reader, "GET", "/api/v1/users/me").await?).await?;
    assert_eq!(me.pointer("/permissions"), Some(&json!(["read_plans"])));
    assert_eq!(me.pointer("/may/read_plans"), Some(&json!("listed")));
    assert_eq!(me.pointer("/campaign_scope"), Some(&json!([published])));

    // The scope narrows the reader, not the campaigns' other readers.
    let team = listed(&state, &f.author, "/api/v1/campaigns", "campaign_id").await?;
    assert_eq!(team.len(), 2);
    Ok(())
}

/// A route open to any active account scopes itself by owner or membership,
/// which would reach past the reader's campaigns; so does every write.
#[rstest]
#[case::vault("GET", "/api/v1/vault/resources")]
#[case::catalog("GET", "/api/v1/orgs/{org}/benchmarks")]
#[case::own_key("DELETE", "/api/v1/users/me/keys/{key}")]
#[tokio::test]
async fn a_campaign_reader_is_refused_a_route_that_scopes_itself(
    #[case] method: &str,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let resp = scope(&state, &f.admin, &f.reader, &[&f.published.0]).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let uri = path
        .replace("{org}", f.org.as_str())
        .replace("{key}", f.reader.2.as_str());
    let resp = signed_empty(&state, &f.reader, method, &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        error_of(resp).await?,
        "a campaign reader reads its campaigns and their plans alone"
    );
    Ok(())
}

#[tokio::test]
async fn a_campaign_reader_files_nothing() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let (published, published_plan) = &f.published;
    scope(&state, &f.admin, &f.reader, &[published]).await?;

    let uri = format!("/api/v1/campaigns/{published}/plans");
    let resp = post_plan_document(&state, &f.reader, &uri, ONE_CELL_PLAN).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let cancel = format!("/api/v1/plans/{published_plan}/cancel");
    let resp = signed_empty(&state, &f.reader, "POST", &cancel).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        error_of(resp).await?,
        "a campaign reader holds read_plans alone, not cancel_plans"
    );
    Ok(())
}

/// Granted `read_plans` alone, and scoped only while it holds nothing else.
#[tokio::test]
async fn a_campaign_reader_holds_read_plans_alone() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    grant_user(&state, &f.reader.1, &[UserPermission::SubmitPlans]).await?;

    let resp = scope(&state, &f.admin, &f.reader, &[&f.published.0]).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        error_of(resp).await?,
        "this account holds submit_plans, which a campaign reader cannot; remove it first"
    );

    grant_user(&state, &f.reader.1, &[UserPermission::ReadPlans]).await?;
    let resp = scope(&state, &f.admin, &f.reader, &[&f.published.0]).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = signed_json(
        &state,
        &f.admin,
        "PATCH",
        &format!("/api/v1/users/{}/permissions", f.reader.1),
        &json!({ "add": ["cancel_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        error_of(resp).await?,
        "a campaign reader cannot hold cancel_plans"
    );
    Ok(())
}

/// A caller scopes an account only to campaigns whose plans it reads, that
/// exist, and that the account's organization owns.
#[tokio::test]
async fn a_campaign_scope_is_bounded_by_the_callers_reach() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (theirs, _) = created_campaign(&state, &stranger, &[("name", "Theirs")]).await?;

    let resp = scope(&state, &f.admin, &f.reader, &[&theirs]).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(error_of(resp).await?.contains(&theirs));

    let root = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let resp = scope(&state, &root, &f.reader, &[&theirs]).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        error_of(resp)
            .await?
            .contains("its own organization's campaigns alone")
    );

    let absent = "campaign_0199e3a1-7c2b-7d40-9a51-3f6c2b8e4d10";
    let resp = scope(&state, &f.admin, &f.reader, &[absent]).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_of(resp).await?, format!("no such campaign: {absent}"));

    let user = state.user_store.get_user(&f.reader.1).await?;
    assert_eq!(
        user.and_then(|user| user.campaign_scope),
        None,
        "nothing was written"
    );
    Ok(())
}

/// No write widens a reader: a scope is replaced, and a reader is no person's.
#[tokio::test]
async fn a_campaign_scope_is_replaced_never_removed() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let (published, _) = &f.published;
    let (kept, _) = &f.kept;
    scope(&state, &f.admin, &f.reader, &[published]).await?;

    let resp = scope(&state, &f.admin, &f.reader, &[kept]).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let campaigns = listed(&state, &f.reader, "/api/v1/campaigns", "campaign_id").await?;
    assert_eq!(campaigns, [kept.as_str()]);

    let uri = format!("/api/v1/users/{}", f.reader.1);
    let resp = signed_json(
        &state,
        &f.admin,
        "PATCH",
        &uri,
        &json!({ "campaign_scope": null }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = signed_json(
        &state,
        &f.admin,
        "PATCH",
        &uri,
        &json!({ "account_type": "human" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        error_of(resp)
            .await?
            .contains("a campaign reader is a service account")
    );

    let user = state.user_store.get_user(&f.reader.1).await?;
    assert_eq!(
        user.and_then(|user| user.campaign_scope)
            .map(|scope| scope.len()),
        Some(1)
    );
    Ok(())
}

#[rstest]
#[case::person(json!({ "account_type": "human" }), "a campaign scope is a service account's")]
#[case::wider(
    json!({ "account_type": "service", "permissions": ["submit_plans"] }),
    "a campaign reader cannot hold submit_plans"
)]
#[tokio::test]
async fn a_campaign_reader_is_created_as_a_service_holding_read_plans(
    #[case] extra: Value,
    #[case] refusal: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let mut body = json!({
        "contact_email": "ops+site@example.test",
        "campaign_scope": [f.published.0],
    });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    let resp = signed_json(&state, &f.admin, "POST", "/api/v1/users", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(error_of(resp).await?.contains(refusal));

    let body = json!({
        "contact_email": "ops+site@example.test",
        "account_type": "service",
        "permissions": ["read_plans"],
        "campaign_scope": [f.published.0],
    });
    let resp = signed_json(&state, &f.admin, "POST", "/api/v1/users", &body).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await?;
    assert_eq!(
        created.pointer("/campaign_scope"),
        Some(&json!([f.published.0]))
    );
    assert_eq!(
        created.pointer("/permissions"),
        Some(&json!(["read_plans"]))
    );
    Ok(())
}

/// `GET /benchmarks` serves a member its organization's private entries; a
/// reader reads its campaigns alone, so it gets the public catalog.
#[tokio::test]
async fn a_campaign_reader_reads_no_private_benchmark() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    let private = pipette_store_testkit::an_org_benchmark()?;
    state
        .catalog_store
        .put_org_benchmark(&f.org, &private)
        .await?;
    let private_id = private.benchmark_id().to_string();
    let catalog = || async {
        let resp = signed_empty(&state, &f.reader, "GET", "/benchmarks").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        anyhow::Ok(benchmark_ids(&body_json(resp).await?))
    };
    assert!(catalog().await?.contains(&private_id), "a member reads it");

    scope(&state, &f.admin, &f.reader, &[&f.published.0]).await?;
    assert!(!catalog().await?.contains(&private_id));
    Ok(())
}

/// A pre-auth key signs in as the reader, so its minter must read every
/// campaign the reader does: a fleet manager outside the organization that
/// does not read its plans mints none.
#[tokio::test]
async fn a_campaign_reader_takes_no_key_from_a_minter_who_reads_less() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    scope(&state, &f.admin, &f.reader, &[&f.published.0]).await?;
    let fleet = registered_user(
        &state,
        &[UserPermission::ManageAnyOrgs, UserPermission::ReadPlans],
    )
    .await?;
    let mint = format!("/api/v1/users/{}/preauth-keys", f.reader.1);

    let resp = signed_json(&state, &fleet, "POST", &mint, &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(error_of(resp).await?.contains(&f.published.0));
    let resp = signed_json(&state, &f.admin, "POST", &mint, &json!({})).await?;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "its organization's admin reads them"
    );
    Ok(())
}

/// A campaign handed to another organization drops out of its readers'
/// reach, though the scope still names it.
#[tokio::test]
async fn a_campaign_reader_loses_a_campaign_handed_to_another_organization() -> anyhow::Result<()> {
    let (_dir, state) = state().await?;
    let f = fixture(&state).await?;
    scope(&state, &f.admin, &f.reader, &[&f.published.0]).await?;
    let read = || async {
        let uri = format!("/api/v1/campaigns/{}", f.published.0);
        anyhow::Ok(signed_empty(&state, &f.reader, "GET", &uri).await?.status())
    };
    assert_eq!(read().await?, StatusCode::OK);

    let labs = seed_org(&state, "labs").await?.org_id;
    let id = pipette_plan_types::CampaignId::try_new(f.published.0.clone())?;
    let mut campaign = state
        .campaign_store
        .get_campaign(&id)
        .await?
        .context("seeded")?;
    campaign.owner = pipette_plan_types::PrincipalOwner::Org { org_id: labs };
    state.campaign_store.put_campaign(&campaign).await?;
    assert_eq!(read().await?, StatusCode::FORBIDDEN);
    Ok(())
}
