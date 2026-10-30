//! What the app's campaign page reads beyond the campaign record: the overview
//! (`GET /api/v1/campaigns/{campaign_id}/overview`), the owner lookup of its hand-off
//! (`GET /api/v1/campaigns/{campaign_id}/owners`), and the variants its edit numbers
//! (`GET /api/v1/campaigns/{campaign_id}/variants`).

use std::time::{Duration, Instant};

use anyhow::Context;
use axum::http::StatusCode;
use httpmock::Method::POST;
use httpmock::{Mock, MockServer};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::types::Mint;
use pipette_httpapi::state::AppState;
use pipette_pipeline::plan_results::JOB_RESULTS_TIMEOUT;
use pipette_plan_types::{CampaignId, OrgId, PlanId, UserId, UserPermission};
use pipette_web_testkit::{
    DUCKDB_TIMEOUT, FULL_EDITOR, MLX_700M_VARIANT, ONE_CELL_PLAN, UserSigner, add_starved_snapshot,
    approved_client, body_json, created_campaign, file_job_bodies, filed_plan, json_str,
    post_plan_document, put_idle_campaign, put_plan_document, registered_user, seed_org,
    seeded_sql_state, seeded_state, set_user_org, signed_empty, signed_json, user_address,
    with_duckdb,
};

async fn read(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = signed_empty(state, signer, "GET", uri).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

/// The overview of `campaign_id` as `signer` reads it, which must answer `200`
/// with a body the app's type describes.
async fn overview(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
) -> anyhow::Result<Value> {
    overview_at(
        state,
        signer,
        &format!("/api/v1/campaigns/{campaign_id}/overview"),
    )
    .await
}

/// [`overview`] with its device-time totals asked for.
async fn overview_with_totals(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
) -> anyhow::Result<Value> {
    let uri = format!("/api/v1/campaigns/{campaign_id}/overview?device_time=true");
    overview_at(state, signer, &uri).await
}

async fn overview_at(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let (status, body) = read(state, signer, uri).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {body}");
    crate::api_schema::assert_conforms("NamedCampaignOverview", &body)?;
    Ok(body)
}

/// A JSON object's keys, sorted, for pinning a wire shape.
fn fields(value: &Value) -> anyhow::Result<Vec<&str>> {
    let mut fields: Vec<&str> = value
        .as_object()
        .context("an answer is an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    Ok(fields)
}

fn history(body: &Value) -> anyhow::Result<&Vec<Value>> {
    body.get("history")
        .and_then(Value::as_array)
        .context("an overview carries its history")
}

/// An account in `org_id` holding exactly `grants`.
async fn member(
    state: &AppState,
    org_id: &OrgId,
    grants: &[UserPermission],
) -> anyhow::Result<UserSigner> {
    let signer = registered_user(state, grants).await?;
    set_user_org(state, &signer.1, Some(org_id.clone())).await?;
    Ok(signer)
}

/// The body that pipette-duckdb returns for one device-time total: `ms`
/// milliseconds, and `unreported` jobs that reported no value.
fn device_time_body(ms: i64, unreported: usize) -> Value {
    json!({
        "columns": ["device_time_ms", "unreported_job_count"],
        "rows": [[ms, unreported]],
    })
}

/// Any `/api/query`, answered as [`device_time_body`].
async fn mock_device_time(server: &MockServer, ms: i64, unreported: usize) -> Mock<'_> {
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(device_time_body(ms, unreported));
        })
        .await
}

fn assert_no_totals(body: &Value) {
    assert_eq!(body["device_time_current_plan"], Value::Null, "{body}");
    assert_eq!(body["device_time_all_plans"], Value::Null, "{body}");
}

/// A campaign of two plans, `first` then `head`, on a state that reads
/// pipette-duckdb at `duckdb`, which answers nothing until a test mocks it.
struct DuckdbCampaign {
    _dir: tempfile::TempDir,
    state: AppState,
    owner: UserSigner,
    duckdb: MockServer,
    campaign_id: String,
    first: PlanId,
    head: PlanId,
}

async fn duckdb_campaign() -> anyhow::Result<DuckdbCampaign> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let duckdb = MockServer::start_async().await;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let head = filed_plan(&state, &owner, &campaign_id, &client_id, "second").await?;
    Ok(DuckdbCampaign {
        _dir: dir,
        state,
        owner,
        duckdb,
        campaign_id,
        first,
        head,
    })
}

// ── GET /api/v1/campaigns/{campaign_id}/overview ───────────────────────────────────

/// The overview is the campaign record, what its administrator may do to it,
/// the plans it ran newest first, each the listing's plan row with its cancel
/// unread, and the names of the owner and the submitters it shows.
#[tokio::test]
async fn test_the_overview_answers_the_campaign_and_its_history() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let head = filed_plan(&state, &owner, &campaign_id, &client_id, "second").await?;
    add_starved_snapshot(&state, &head).await?;
    let resp = signed_empty(
        &state,
        &owner,
        "POST",
        &format!("/api/v1/plans/{head}/cancel"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let body = overview_with_totals(&state, &owner, &campaign_id).await?;
    assert_eq!(
        fields(&body)?,
        [
            "campaign_id",
            "catalog_org_id",
            "created_at",
            "current_plan_id",
            "device_time_all_plans",
            "device_time_current_plan",
            "history",
            "history_truncated",
            "may_administer",
            "may_edit_variants",
            "may_rerun",
            "name",
            "names",
            "owner",
            "updated_at",
        ]
    );
    assert_eq!(body["name"], json!("Nightly"));
    assert_eq!(body["current_plan_id"], json!(head.as_str()));
    assert_eq!(body["may_administer"], json!(true));
    assert_eq!(body["may_edit_variants"], json!(true));
    assert_eq!(body["may_rerun"], json!(true));
    assert_eq!(body["history_truncated"], json!(false));
    // No `[duckdb]` is configured.
    assert_no_totals(&body);

    let rows = history(&body)?;
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|row| row.get("plan_id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, [head.as_str(), first.as_str()]);
    let plan = &rows[0]["plan"];
    assert_eq!(
        fields(plan)?,
        [
            "campaign_id",
            "cancel_requested",
            "counts",
            "created_at",
            "plan_id",
            "status",
            "submitted_by",
        ],
        "a history row is a plan summary: {plan}"
    );
    assert_eq!(
        plan["cancel_requested"],
        json!(false),
        "a history row reads no cancel marker"
    );
    // The plan read reports the cancel; the rest is the listing's row.
    let mut expected = crate::campaign_listings::as_a_row(&state, &owner, &head).await?;
    expected["cancel_requested"] = json!(false);
    assert_eq!(plan, &expected);
    assert_eq!(plan["counts"]["total"], json!(1));

    // The record alone, which the document page heads itself with.
    let (status, record) =
        read(&state, &owner, &format!("/api/v1/campaigns/{campaign_id}")).await?;
    assert_eq!(status, StatusCode::OK);
    crate::api_schema::assert_conforms("NamedCampaignView", &record)?;

    let address = user_address(&state, &owner.1).await?;
    assert_eq!(
        body["names"],
        json!({ owner.1.as_str(): address }),
        "the owner, who filed both plans, by address"
    );
    Ok(())
}

/// A member of the owning organization is offered the revisions it may file,
/// and no write to the record.
#[tokio::test]
async fn test_the_overview_offers_a_member_the_revisions_and_not_the_record() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let author = member(&state, &acme, FULL_EDITOR).await?;
    let params = [("name", "Team"), ("owner", acme.as_str())];
    let (campaign_id, _) = created_campaign(&state, &author, &params).await?;

    let colleague = member(&state, &acme, FULL_EDITOR).await?;
    let body = overview(&state, &colleague, &campaign_id).await?;
    assert_eq!(body["may_administer"], json!(false));
    assert_eq!(body["may_edit_variants"], json!(true));
    assert_eq!(body["may_rerun"], json!(true));

    // A revision ends the plan it replaces, which takes `cancel_plans`.
    let filer = member(
        &state,
        &acme,
        &[UserPermission::SubmitPlans, UserPermission::ReadClients],
    )
    .await?;
    let body = overview(&state, &filer, &campaign_id).await?;
    assert_eq!(body["may_edit_variants"], json!(false));
    Ok(())
}

/// A campaign in reach whose plans the reader may not read names them by id
/// alone, says it names a plan, and names no submitter. Its organization's
/// administrator may administer it all the same, and edit no variant without
/// the plan grants.
#[tokio::test]
async fn test_the_overview_withholds_what_the_plan_scope_does_not_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let duckdb = MockServer::start_async().await;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let totals = mock_device_time(&duckdb, 1_000, 0).await;
    let acme = seed_org(&state, "acme").await?.org_id;
    let author = member(
        &state,
        &acme,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    let (campaign_id, _) = created_campaign(
        &state,
        &author,
        &[("name", "Team"), ("owner", acme.as_str())],
    )
    .await?;
    filed_plan(&state, &author, &campaign_id, &client_id, "second").await?;

    let lead = member(&state, &acme, &[UserPermission::ManageOrgs]).await?;
    let body = overview_with_totals(&state, &lead, &campaign_id).await?;
    assert!(body["current_plan_id"].is_string(), "{body}");
    assert_eq!(body["may_administer"], json!(true));
    assert_eq!(body["may_edit_variants"], json!(false));
    assert_eq!(body["may_rerun"], json!(false));
    let rows = history(&body)?;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["plan"].is_null()), "{body}");
    assert_eq!(body["names"], json!({ acme.as_str(): "acme" }));
    // A total over a plan the reader may not read is not asked for.
    assert_no_totals(&body);
    totals.assert_calls_async(0).await;

    // A member reads the plans, and administers nothing.
    let reader = member(&state, &acme, &[UserPermission::ReadPlans]).await?;
    let body = overview_with_totals(&state, &reader, &campaign_id).await?;
    assert_eq!(body["may_administer"], json!(false));
    assert!(history(&body)?.iter().all(|row| row["plan"].is_object()));
    let total = json!({ "device_time_ms": 1_000, "unreported_job_count": 0 });
    assert_eq!(body["device_time_current_plan"], total);
    assert_eq!(body["device_time_all_plans"], total);
    totals.assert_calls_async(2).await;

    // Unasked, as the plan page reads it, the overview sends no query.
    let body = overview(&state, &reader, &campaign_id).await?;
    assert_no_totals(&body);
    totals.assert_calls_async(2).await;
    let mut named = vec![acme.as_str(), author.1.as_str()];
    named.sort_unstable();
    assert_eq!(
        fields(&body["names"])?,
        named,
        "the owner and the submitter"
    );
    Ok(())
}

/// The current-plan total sums the jobs of the head, re-scored copies included.
/// The all-plans total sums each distinct job of every plan, re-scored copies
/// left out.
#[tokio::test]
async fn test_the_overview_totals_the_device_time_of_its_plans() -> anyhow::Result<()> {
    let DuckdbCampaign {
        _dir,
        state,
        owner,
        duckdb,
        campaign_id,
        first,
        head,
    } = duckdb_campaign().await?;
    let current_plan = duckdb
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .body_includes(format!("IN ('{head}')"))
                .body_excludes("rescored_from IS NULL");
            then.status(200).json_body(device_time_body(184_250, 1));
        })
        .await;
    let all_plans = duckdb
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .body_includes(format!("IN ('{head}', '{first}')"))
                .body_includes("rescored_from IS NULL");
            then.status(200).json_body(device_time_body(600_000, 2));
        })
        .await;

    let body = overview_with_totals(&state, &owner, &campaign_id).await?;
    assert_eq!(
        body["device_time_current_plan"],
        json!({ "device_time_ms": 184_250, "unreported_job_count": 1 })
    );
    assert_eq!(
        body["device_time_all_plans"],
        json!({ "device_time_ms": 600_000, "unreported_job_count": 2 })
    );
    current_plan.assert_async().await;
    all_plans.assert_async().await;
    Ok(())
}

/// A failing pipette-duckdb costs the overview its totals, not the campaign.
#[tokio::test]
async fn test_the_overview_answers_without_totals_when_duckdb_fails() -> anyhow::Result<()> {
    let c = duckdb_campaign().await?;
    let failing = c
        .duckdb
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(400)
                .json_body(json!({"error": "Binder Error: column device_time_ms not found"}));
        })
        .await;

    let body = overview_with_totals(&c.state, &c.owner, &c.campaign_id).await?;
    assert_eq!(history(&body)?.len(), 2);
    assert_no_totals(&body);
    failing.assert_calls_async(2).await;
    Ok(())
}

/// A hung pipette-duckdb holds the overview for [`JOB_RESULTS_TIMEOUT`], not for
/// the testkit's [`DUCKDB_TIMEOUT`].
#[tokio::test]
async fn test_the_overview_answers_without_totals_when_duckdb_hangs() -> anyhow::Result<()> {
    let c = duckdb_campaign().await?;
    c.duckdb
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.delay(Duration::from_secs(30)).status(200);
        })
        .await;

    let started = Instant::now();
    let body = overview_with_totals(&c.state, &c.owner, &c.campaign_id).await?;
    let waited = started.elapsed();
    assert!(
        waited >= JOB_RESULTS_TIMEOUT && waited < DUCKDB_TIMEOUT,
        "{waited:?}"
    );
    assert_eq!(history(&body)?.len(), 2);
    assert_no_totals(&body);
    Ok(())
}

/// A campaign that names no plan says so with an empty history; one outside
/// the reader's reach is refused as its record is.
#[tokio::test]
async fn test_the_overview_keeps_the_campaign_s_reach() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let idle = put_idle_campaign(&state, &owner.1, "Not started").await?;
    let body = overview_at(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{idle}/overview"),
    )
    .await?;
    assert_eq!(history(&body)?.len(), 0);
    assert!(body.get("current_plan_id").is_none(), "{body}");

    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (status, _) = read(
        &state,
        &stranger,
        &format!("/api/v1/campaigns/{idle}/overview"),
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = read(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{}/overview", CampaignId::mint()),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = read(&state, &owner, "/api/v1/campaigns/nope!/overview").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

/// An overview always carries `names`, an empty object when no name reads.
#[tokio::test]
async fn test_the_overview_names_nobody_as_an_empty_object() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let reader = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    let orphan = put_idle_campaign(&state, &UserId::mint(), "Orphan").await?;
    let body = overview(&state, &reader, orphan.as_str()).await?;
    assert_eq!(body.get("names"), Some(&json!({})), "{body}");
    Ok(())
}

// ── GET /api/v1/campaigns/{campaign_id}/owners ─────────────────────────────────────

/// The values an owner lookup answered, in order, after checking its schema.
fn offered(body: &Value) -> anyhow::Result<Vec<String>> {
    crate::api_schema::assert_conforms("OwnerMatches", body)?;
    Ok(body
        .get("owners")
        .and_then(Value::as_array)
        .context("an answer carries owners")?
        .iter()
        .filter_map(|owner| {
            owner
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

/// `signer`'s answer from the campaign's owner lookup, with `query`.
async fn owners(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    query: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    read(
        state,
        signer,
        &format!("/api/v1/campaigns/{campaign_id}/owners?{query}"),
    )
    .await
}

/// The signed lookup answers what the console's hand-off field offers: a member
/// its own organization alone, the organization's administrator its members
/// who file plans, never the current owner. A campaign no longer the reader's
/// to give is a `403`, and one it may not read is refused as its read is.
#[tokio::test]
async fn test_the_owner_lookup_offers_where_the_campaign_may_go() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let rival = seed_org(&state, "rival").await?.org_id;
    let author = member(&state, &acme, &[UserPermission::SubmitPlans]).await?;
    let colleague = member(&state, &acme, &[UserPermission::SubmitPlans]).await?;
    let idle = member(&state, &acme, &[]).await?;
    let outsider = member(&state, &rival, &[UserPermission::SubmitPlans]).await?;
    let lead = member(&state, &acme, &[UserPermission::ManageOrgs]).await?;
    let (campaign_id, _) = created_campaign(&state, &author, &[("name", "Handed")]).await?;

    let (status, body) = owners(&state, &author, &campaign_id, "q=").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(offered(&body)?, [acme.to_string()]);

    // The app posts the picked value to the signed hand-off.
    let request = json!({ "owner": { "type": "org", "org_id": acme } });
    crate::api_schema::assert_conforms("SetCampaignOwnerRequest", &request)?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");
    let resp = signed_json(&state, &author, "PUT", &uri, &request).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let (status, _) = owners(&state, &author, &campaign_id, "q=").await?;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "no longer the author's to give"
    );

    let (_, body) = owners(&state, &lead, &campaign_id, "q=").await?;
    let values = offered(&body)?;
    for (principal, wanted) in [
        (colleague.1.to_string(), true),
        (author.1.to_string(), true),
        (idle.1.to_string(), false),
        (acme.to_string(), false),
        (rival.to_string(), false),
        (outsider.1.to_string(), false),
    ] {
        assert_eq!(values.contains(&principal), wanted, "{principal}: {body}");
    }

    let (_, body) = owners(&state, &lead, &campaign_id, "q=&type=org").await?;
    assert_eq!(offered(&body)?, Vec::<String>::new());
    let typed = user_address(&state, &colleague.1).await?.to_uppercase();
    let (_, body) = owners(&state, &lead, &campaign_id, &format!("q={typed}")).await?;
    assert_eq!(offered(&body)?, [colleague.1.to_string()]);
    let (status, _) = owners(&state, &lead, &campaign_id, "type=device").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = owners(&state, &outsider, &campaign_id, "q=").await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "outside the reach");
    let (status, _) = owners(&state, &lead, CampaignId::mint().as_str(), "q=").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

// ── GET /api/v1/campaigns/{campaign_id}/variants ───────────────────────────────────

/// The listing names the plan the campaign runs and its variants as an edit
/// numbers them, and follows the campaign onto the revision an edit files.
#[tokio::test]
async fn test_the_variants_follow_the_head() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/variants");

    let (status, body) = read(&state, &owner, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    crate::api_schema::assert_conforms("CampaignVariants", &body)?;
    assert_eq!(body["base"]["plan_id"], json!(first.as_str()));
    let variants = body["variants"].as_array().context("variants")?;
    assert_eq!(variants.len(), 1);
    assert!(
        variants[0]
            .as_str()
            .is_some_and(|block| block.contains("LFM2.5-350M-MLX-4bit")),
        "{body}"
    );

    // The preview the form draws, then the edit it files.
    let edit = format!("{uri}?base_plan_id={first}&note=rev-1");
    let resp = post_plan_document(
        &state,
        &owner,
        &format!("{edit}&dry_run=true"),
        MLX_700M_VARIANT,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    crate::api_schema::assert_conforms("RevisionDryRun", &body_json(resp).await?)?;
    let resp = post_plan_document(&state, &owner, &edit, MLX_700M_VARIANT).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let revision = json_str(&body_json(resp).await?["plan"], "plan_id")?.to_string();

    let (_, body) = read(&state, &owner, &uri).await?;
    assert_eq!(body["base"]["plan_id"], json!(revision));
    assert_eq!(body["variants"].as_array().map(Vec::len), Some(2));
    Ok(())
}

/// Who the edit would refuse, the listing and the document read refuse at the
/// same status: another owner's campaign, a reader below the full tier, a
/// campaign that runs no plan, a plan filed as job bodies, an unknown or
/// malformed id, and a reader holding no campaign grant before it learns
/// whether the campaign exists.
#[rstest]
#[case::stranger("stranger", StatusCode::FORBIDDEN)]
#[case::withheld_tier("withheld", StatusCode::FORBIDDEN)]
#[case::runs_no_plan("idle", StatusCode::CONFLICT)]
#[case::job_bodies("bodies", StatusCode::CONFLICT)]
#[case::unknown("unknown", StatusCode::NOT_FOUND)]
#[case::malformed("malformed", StatusCode::BAD_REQUEST)]
#[case::ungranted_unknown("ungranted", StatusCode::FORBIDDEN)]
#[tokio::test]
async fn test_the_variants_are_refused_as_the_edit_is(
    #[case] who: &str,
    #[case] status: StatusCode,
    #[values("variants", "document")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, _) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let (reader, campaign_id) = match who {
        "stranger" => (registered_user(&state, FULL_EDITOR).await?, campaign_id),
        "withheld" => {
            let reader = registered_user(
                &state,
                &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
            )
            .await?;
            let (campaign_id, _) =
                created_campaign(&state, &reader, &[("name", "Withheld")]).await?;
            (reader, campaign_id)
        }
        "idle" => {
            let idle = put_idle_campaign(&state, &owner.1, "Not started").await?;
            (owner, idle.to_string())
        }
        "unknown" => (owner, CampaignId::mint().to_string()),
        "malformed" => (owner, "nope!".to_string()),
        "ungranted" => (
            registered_user(&state, &[UserPermission::ReadPlans]).await?,
            CampaignId::mint().to_string(),
        ),
        _ => {
            file_job_bodies(&state, &owner, &campaign_id, &client_id, "bodies").await?;
            (owner, campaign_id)
        }
    };
    let (answered, body) = read(
        &state,
        &reader,
        &format!("/api/v1/campaigns/{campaign_id}/{route}"),
    )
    .await?;
    assert_eq!(answered, status, "{body}");
    Ok(())
}

// ── GET /api/v1/campaigns/{campaign_id}/document ───────────────────────────────────

/// The read names the plan the campaign runs and the document it was filed
/// from, as written, and follows the campaign onto the revision the form files.
#[tokio::test]
async fn test_the_document_follows_the_head() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/document");

    let (status, body) = read(&state, &owner, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    crate::api_schema::assert_conforms("CampaignDocument", &body)?;
    assert_eq!(body["base"]["plan_id"], json!(first.as_str()));
    assert_eq!(body["document"], json!(ONE_CELL_PLAN));

    // The preview the form draws, then the revision it files.
    let revised = format!("# with the 700M model\n{ONE_CELL_PLAN}{MLX_700M_VARIANT}");
    let file = format!("{uri}?base_plan_id={first}&note=rev-1");
    let resp = put_plan_document(&state, &owner, &format!("{file}&dry_run=true"), &revised).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let preview = body_json(resp).await?;
    crate::api_schema::assert_conforms("RevisionDryRun", &preview)?;
    assert_eq!(
        (
            &preview["unchanged"],
            &preview["carried_job_count"],
            &preview["new_job_count"],
            &preview["dropped_job_ids"]
        ),
        (&json!(false), &json!(1), &json!(1), &json!([]))
    );
    let resp = put_plan_document(&state, &owner, &file, &revised).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let revision = json_str(&body_json(resp).await?["plan"], "plan_id")?.to_string();

    let (_, body) = read(&state, &owner, &uri).await?;
    assert_eq!(body["base"]["plan_id"], json!(revision));
    assert_eq!(body["document"], json!(revised));
    Ok(())
}

/// A rename the app sends conforms to the request type the route reads.
#[tokio::test]
async fn test_the_rename_request_conforms() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (campaign_id, _) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let request = json!({ "name": "Nightly v2" });
    crate::api_schema::assert_conforms("UpdateCampaignRequest", &request)?;
    let resp = signed_json(
        &state,
        &owner,
        "PATCH",
        &format!("/api/v1/campaigns/{campaign_id}"),
        &request,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        overview(&state, &owner, &campaign_id).await?["name"],
        json!("Nightly v2")
    );
    Ok(())
}

/// The grants of the account that creates the campaign in the catalog
/// organization tests.
const CATALOG_AUTHOR: [UserPermission; 3] = [
    UserPermission::SubmitPlans,
    UserPermission::ReadPlans,
    UserPermission::EditCatalog,
];

/// The owner of the campaign in [`test_the_overview_names_the_catalog_org`].
#[derive(Clone, Copy, Debug)]
enum Owned {
    /// The organization `acme`.
    Acme,
    /// The author, a member of `acme`.
    AcmeMember,
    /// The author, a member of no organization.
    Unaffiliated,
}

/// The reader in [`test_the_overview_names_the_catalog_org`].
#[derive(Clone, Copy, Debug)]
enum Reader {
    /// The account that created the campaign, which holds `edit_catalog`.
    Author,
    AcmeEditor,
    AcmeWithoutEdit,
    /// A member of `acme` that reads every campaign, including the campaign of
    /// a colleague.
    AcmeEditorReadingAny,
    BetaAnyEditor,
    UnaffiliatedAnyEditor,
}

/// The organization where the reader may create a descriptor catalog entry is
/// the campaign's organization. The reader gets it only if it is the reader's
/// own organization, or if the reader has no organization and holds
/// `edit_any_catalog`.
#[rstest]
#[case::org_member_with_edit_catalog(Owned::Acme, Reader::AcmeEditor, true)]
#[case::org_member_without_edit_catalog(Owned::Acme, Reader::AcmeWithoutEdit, false)]
#[case::other_org_member_with_edit_any(Owned::Acme, Reader::BetaAnyEditor, false)]
#[case::unaffiliated_edit_any(Owned::Acme, Reader::UnaffiliatedAnyEditor, true)]
#[case::user_owned_owner(Owned::AcmeMember, Reader::Author, true)]
#[case::user_owned_org_mate(Owned::AcmeMember, Reader::AcmeEditorReadingAny, true)]
#[case::user_owned_other_org_edit_any(Owned::AcmeMember, Reader::BetaAnyEditor, false)]
#[case::user_owned_unaffiliated_edit_any(Owned::AcmeMember, Reader::UnaffiliatedAnyEditor, true)]
#[case::owner_in_no_org(Owned::Unaffiliated, Reader::Author, false)]
#[case::owner_in_no_org_edit_any(Owned::Unaffiliated, Reader::UnaffiliatedAnyEditor, false)]
#[tokio::test]
async fn test_the_overview_names_the_catalog_org(
    #[case] owned: Owned,
    #[case] reader: Reader,
    #[case] in_acme: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let acme = seed_org(state, "acme").await?.org_id;
    let beta = seed_org(state, "beta").await?.org_id;
    let author = match owned {
        Owned::Acme | Owned::AcmeMember => member(state, &acme, &CATALOG_AUTHOR).await?,
        Owned::Unaffiliated => registered_user(state, &CATALOG_AUTHOR).await?,
    };
    let params: &[(&str, &str)] = match owned {
        Owned::Acme => &[("name", "Team"), ("owner", acme.as_str())],
        Owned::AcmeMember | Owned::Unaffiliated => &[("name", "Own")],
    };
    let (campaign_id, _) = created_campaign(state, &author, params).await?;

    let edit = UserPermission::EditCatalog;
    let read_any = UserPermission::ReadAnyPlans;
    let edit_any = UserPermission::EditAnyCatalog;
    let signer = match reader {
        Reader::Author => author,
        Reader::AcmeEditor => member(state, &acme, &[UserPermission::ReadPlans, edit]).await?,
        Reader::AcmeWithoutEdit => member(state, &acme, &[UserPermission::ReadPlans]).await?,
        Reader::AcmeEditorReadingAny => member(state, &acme, &[read_any, edit]).await?,
        Reader::BetaAnyEditor => member(state, &beta, &[read_any, edit_any]).await?,
        Reader::UnaffiliatedAnyEditor => registered_user(state, &[read_any, edit_any]).await?,
    };
    let body = overview(state, &signer, &campaign_id).await?;
    let expected = if in_acme {
        json!(acme.as_str())
    } else {
        Value::Null
    };
    assert_eq!(
        body["catalog_org_id"], expected,
        "{owned:?} read by {reader:?}"
    );
    Ok(())
}

/// A server without a `[database]` catalog names no catalog organization, so
/// the page offers no catalog entry.
#[tokio::test]
async fn test_the_overview_names_no_catalog_org_without_a_catalog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let author = member(&state, &acme, &CATALOG_AUTHOR).await?;
    let params = [("name", "Team"), ("owner", acme.as_str())];
    let (campaign_id, _) = created_campaign(&state, &author, &params).await?;
    let body = overview(&state, &author, &campaign_id).await?;
    assert_eq!(body["catalog_org_id"], Value::Null, "{body}");
    Ok(())
}
