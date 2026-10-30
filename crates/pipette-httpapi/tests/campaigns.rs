//! The campaign surface end to end: CRUD under `/api/v1/campaigns`, the create
//! that files a first plan, revisions that chain a new plan onto the campaign,
//! the ownership rule on every write, and reads that disclose no plan the
//! reader could not already read.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};
use tokio::sync::Notify;

use pipette_core::campaign::{Campaign, CampaignOwner};
use pipette_core::types::Mint;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    CampaignId, JobId, PlanId, PlanStatus, UserId, UserPermission, UserStatus,
};
use pipette_store::{CampaignPage, CampaignStore, PlanStore};
use pipette_web_testkit::*;

// ── fixtures ────────────────────────────────────────────────────────────────

/// The upstream scheduler example, which expands to three cells.
const AFM_MLX: &str = include_str!("../../../examples/plans/scheduler/afm-mlx.toml");

/// A server admin: creates organizations, and reaches every campaign.
async fn server_admin(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(
        state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::SubmitAnyPlans,
            UserPermission::ReadAnyPlans,
        ],
    )
    .await
}

/// An ordinary operator: files and reads its own work, administers nobody.
async fn operator(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, &[UserPermission::SubmitPlans]).await
}

/// An `org_admin` on `org_id`, holding no plan grant: administering a team's
/// campaigns is a roster authority, not a plan one.
async fn org_admin(state: &AppState, org_id: &str) -> anyhow::Result<UserSigner> {
    let member = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(state, &member.1, Some(org_id.parse()?)).await?;
    Ok(member)
}

/// The manifest a plan id names, read straight from the store: the chain is a
/// property of what was written, not of what a route chose to show.
async fn manifest(
    state: &AppState,
    plan_id: &PlanId,
) -> anyhow::Result<pipette_core::plan::PlanManifest> {
    state
        .plan_store
        .get_plan(plan_id)
        .await?
        .context("the manifest was written")
}

/// `owner` is `None` for a campaign the caller owns itself.
async fn create_campaign(
    state: &AppState,
    signer: &UserSigner,
    name: &str,
    owner: Option<&str>,
) -> anyhow::Result<String> {
    let mut params = vec![("name", name)];
    params.extend(owner.map(|owner| ("owner", owner)));
    Ok(created_campaign(state, signer, &params).await?.0)
}

/// The campaigns in the store, whoever owns them: what a refused create must
/// leave empty.
async fn stored_campaigns(state: &AppState) -> anyhow::Result<usize> {
    Ok(
        pipette_store::drain_campaigns(state.campaign_store.as_ref())
            .await?
            .items
            .len(),
    )
}

/// A well-formed plan id nobody minted, for the tests about what a route refuses
/// rather than what it files.
fn a_plan_id() -> PlanId {
    PlanId::from_uuid(uuid::Uuid::now_v7())
}

/// A well-formed campaign id nobody minted, on the same rule.
fn an_unminted_campaign() -> String {
    "campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e".to_string()
}

/// Rename `campaign_id`, returning the raw response: the ownership tests use the
/// cheapest write there is, because what they are about is who may make one.
async fn rename(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    name: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        signer,
        "PATCH",
        &format!("/api/v1/campaigns/{campaign_id}"),
        &json!({ "name": name }),
    )
    .await
}

/// `PATCH` `uri` with `body`, checked against its schema, expecting a `200`.
async fn patch_campaign(
    state: &AppState,
    owner: &UserSigner,
    uri: &str,
    body: Value,
) -> anyhow::Result<Value> {
    crate::api_schema::assert_conforms("UpdateCampaignRequest", &body)?;
    let resp = signed_json(state, owner, "PATCH", uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    body_json(resp).await
}

/// The plan ids the `GET /api/v1/plans` listing at `uri` answers `signer` with.
async fn listed_plans(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
) -> anyhow::Result<Vec<String>> {
    let (key, user_id, key_id) = signer;
    let resp = authed_user_get(state, key, user_id, key_id, uri).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "listing the plans failed with {}",
        resp.status()
    );
    plan_ids(&body_json(resp).await?)
}

/// The campaign ids a listing body carries, in the order it returned them.
fn campaign_ids(body: &Value) -> anyhow::Result<Vec<String>> {
    body["items"]
        .as_array()
        .context("a listing carries an `items` array")?
        .iter()
        .map(|row| Ok(json_str(row, "campaign_id")?.to_string()))
        .collect()
}

/// `GET uri` as `signer`: its status, and its body.
async fn get_as(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let (key, user_id, key_id) = signer;
    let resp = authed_user_get(state, key, user_id, key_id, uri).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

/// Every read of one campaign. `{campaign}` stands for the campaign.
const ONE_CAMPAIGN_READS: [&str; 3] = [
    "/api/v1/campaigns/{campaign}",
    "/api/v1/campaigns/{campaign}/plan",
    "/api/v1/campaigns/{campaign}/plans",
];

fn one_campaign_reads(campaign_id: &str) -> Vec<String> {
    ONE_CAMPAIGN_READS
        .iter()
        .map(|uri| uri.replace("{campaign}", campaign_id))
        .collect()
}

// ── the create ──────────────────────────────────────────────────────────────

/// Creating a campaign files its first plan: the plan belongs to the campaign,
/// supersedes nothing, keeps its document, and is attributed to the caller, and
/// the campaign is written running it.
#[tokio::test]
async fn creating_a_campaign_files_its_first_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;

    let resp = post_campaign(
        &state,
        &owner,
        &[("name", "Nightly LFM2"), ("note", "nightly-1")],
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    let campaign_id = json_str(&body, "campaign_id")?.to_string();
    assert_eq!(body["name"], json!("Nightly LFM2"));
    assert_eq!(body["owner"]["user_id"], json!(owner.1.as_str()));
    assert_eq!(body["plan"]["note"], json!("nightly-1"));
    assert_eq!(body["plan"]["job_count"], json!(1));
    let plan_id = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    assert_eq!(body["current_plan_id"], json!(plan_id.as_str()));

    let manifest = manifest(&state, &plan_id).await?;
    assert_eq!(
        manifest.campaign_id.as_ref().map(|id| id.as_str()),
        Some(campaign_id.as_str()),
        "the plan records the campaign it was created with"
    );
    assert_eq!(manifest.previous_plan_id, None);
    assert_eq!(manifest.submitted_by.as_ref(), Some(&owner.1));
    assert!(manifest.source_sha256.is_some(), "the document is stored");

    let (key, user_id, key_id) = &owner;
    let read = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(read["current_plan_id"], json!(plan_id.as_str()));
    Ok(())
}

#[tokio::test]
async fn a_dry_run_create_writes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;

    let resp = post_campaign(&state, &owner, &[("name", "Nightly"), ("dry_run", "true")]).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["bodies"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await?;
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// The refusal is the document's own.
#[tokio::test]
async fn a_refused_document_creates_no_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;

    let resp = post_plan_document(
        &state,
        &owner,
        "/api/v1/campaigns?name=Nightly",
        "plan_id = \"drafted\"\n",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("plan_id"), "{error}");
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// Creating a campaign files a plan, so it takes `submit_plans` even for an
/// organization's administrator, who administers the team's campaigns without
/// it.
#[tokio::test]
async fn creating_a_campaign_takes_submit_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &acme).await?;

    let resp = post_campaign(
        &state,
        &team_admin,
        &[("name", "Release gate"), ("owner", acme.as_str())],
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("submit_plans"), "{error}");
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// Whom an out-of-scope create names as the owner.
#[derive(Clone, Copy, Debug)]
enum ForeignOwner {
    AnotherAccount,
    AnOrganizationTheCallerIsNotOn,
}

/// A create naming an owner outside the caller's scope is refused before the
/// plan is filed. The caller holds `submit_plans` and the document is valid, so
/// only the owner check stands between the request and a staged plan.
#[rstest]
#[case::another_account(ForeignOwner::AnotherAccount)]
#[case::an_organization_the_caller_is_not_on(ForeignOwner::AnOrganizationTheCallerIsNotOn)]
#[tokio::test]
async fn creating_for_an_owner_out_of_scope_files_nothing(
    #[case] foreign: ForeignOwner,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let caller = operator(&state).await?;
    let owner = match foreign {
        ForeignOwner::AnotherAccount => operator(&state).await?.1.to_string(),
        ForeignOwner::AnOrganizationTheCallerIsNotOn => create_org(&state, &admin, "acme").await?,
    };

    let resp = post_campaign(&state, &caller, &[("name", "Theirs"), ("owner", &owner)]).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{foreign:?}");
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await
}

/// A member holding `submit_plans` without `manage_orgs` creates its
/// organization's campaign and files into it, as a colleague does. Its record
/// is the organization's administrators': the creator renames nothing.
#[tokio::test]
async fn a_member_files_into_its_teams_campaign_and_does_not_administer_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let member = operator(&state).await?;
    let colleague = operator(&state).await?;
    for signer in [&member, &colleague] {
        set_user_org(&state, &signer.1, Some(acme.parse()?)).await?;
    }

    let (campaign_id, first) =
        created_campaign(&state, &member, &[("name", "Team"), ("owner", &acme)]).await?;
    assert_eq!(
        manifest(&state, &first).await?.submitted_by.as_ref(),
        Some(&member.1)
    );

    let uri = format!("/api/v1/campaigns/{campaign_id}/plans");
    for signer in [&member, &colleague] {
        let resp = post_plan_document(&state, signer, &uri, ONE_CELL_PLAN).await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let revision = head(&state, &campaign_id)
            .await?
            .context("the revision is the head")?;
        assert_eq!(
            manifest(&state, &revision).await?.submitted_by.as_ref(),
            Some(&signer.1)
        );
    }

    let resp = rename(&state, &member, &campaign_id, "Renamed").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let error = body_json(resp).await?;
    assert!(json_str(&error, "error")?.contains("needs manage_orgs"));
    // Nor does it take the team's campaign for itself.
    let to_self = json!({ "type": "user", "user_id": member.1.as_str() });
    assert_eq!(
        hand_over(&state, &member, &campaign_id, to_self).await?,
        StatusCode::FORBIDDEN
    );

    // An account on no organization, or on another, files nothing into it.
    let stranger = operator(&state).await?;
    let rival = operator(&state).await?;
    let other = create_org(&state, &admin, "rival").await?;
    set_user_org(&state, &rival.1, Some(other.parse()?)).await?;
    for signer in [&stranger, &rival] {
        let resp = post_plan_document(&state, signer, &uri, ONE_CELL_PLAN).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    Ok(())
}

/// Creating a campaign for an account without `submit_plans` is refused, whoever
/// creates it, and files nothing; granting it lets the create through.
#[tokio::test]
async fn creating_for_an_account_without_submit_plans_files_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let team_lead = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    let member = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    for signer in [&team_lead, &member] {
        set_user_org(&state, &signer.1, Some(acme.parse()?)).await?;
    }
    let params = [("name", "Handed down"), ("owner", member.1.as_str())];

    for caller in [&team_lead, &admin] {
        let resp = post_campaign(&state, caller, &params).await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains("owner needs submit_plans"), "{error}");
    }
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await?;

    grant_user(
        &state,
        &member.1,
        &[UserPermission::ReadPlans, UserPermission::SubmitPlans],
    )
    .await?;
    let resp = post_campaign(&state, &team_lead, &params).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await?["owner"]["user_id"],
        json!(member.1.as_str())
    );
    Ok(())
}

/// A campaign store whose writes signal `entered`, then wait for `release`: the
/// window between a route's plan write and its campaign write, held open.
struct GatedPut {
    inner: Arc<dyn CampaignStore>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl CampaignStore for GatedPut {
    async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
        self.inner.get_campaign(campaign_id).await
    }
    async fn put_campaign(&self, campaign: &Campaign) -> anyhow::Result<()> {
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.put_campaign(campaign).await
    }
    async fn list_campaigns(
        &self,
        after: Option<&CampaignId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage> {
        self.inner.list_campaigns(after, limit).await
    }
    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
        self.inner.delete_campaign(campaign_id).await
    }
}

/// The two routes that write a plan and then the campaign.
#[derive(Clone, Copy, Debug)]
enum TwoWrites {
    /// `POST /api/v1/campaigns`: the first plan, then the record.
    Create,
    /// `POST /api/v1/campaigns/{campaign_id}/plans`: a revision, then the advance.
    Revision,
}

#[rstest]
#[case::create(TwoWrites::Create)]
#[case::revision(TwoWrites::Revision)]
#[tokio::test]
async fn a_disconnect_between_the_writes_still_writes_the_campaign(
    #[case] route: TwoWrites,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let (uri, first) = match route {
        TwoWrites::Create => ("/api/v1/campaigns?name=Nightly".to_string(), None),
        TwoWrites::Revision => {
            let (campaign_id, first) =
                created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
            (
                format!("/api/v1/campaigns/{campaign_id}/plans"),
                Some(first),
            )
        }
    };
    let inner = state.campaign_store.clone();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    state.campaign_store = Arc::new(GatedPut {
        inner: inner.clone(),
        entered: entered.clone(),
        release: release.clone(),
    });

    // Hold the request at its campaign write, then drop it as a disconnect does.
    tokio::select! {
        () = entered.notified() => {}
        resp = post_plan_document(&state, &owner, &uri, ONE_CELL_PLAN) => {
            anyhow::bail!("{route:?} finished before its campaign write: {:?}", resp?.status());
        }
    }
    release.notify_one();

    let landed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let heads = pipette_store::drain_campaigns(inner.as_ref())
                .await?
                .items
                .into_iter()
                .filter_map(|campaign| campaign.current_plan_id)
                .filter(|head| Some(head) != first.as_ref())
                .count();
            if heads > 0 {
                return anyhow::Ok(heads);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("the campaign write never landed")??;
    assert_eq!(landed, 1, "{route:?}");
    Ok(())
}

/// A campaign write that fails after the first plan is filed is a `500` with
/// no store detail, and the plan it strands is cancelled rather than left
/// claimable under a campaign id nothing holds.
#[tokio::test]
async fn a_failed_campaign_write_cancels_the_first_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    state.campaign_store = std::sync::Arc::new(pipette_store_testkit::CampaignStoreUnimplemented);

    let resp = post_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(!error.contains("put_campaign"), "{error}");

    let plans = pipette_store::drain_plans(state.plan_store.as_ref(), None)
        .await?
        .items;
    let [plan] = plans.as_slice() else {
        anyhow::bail!("expected the one staged plan, found {}", plans.len());
    };
    assert!(state.plan_store.has_cancel_marker(&plan.plan_id).await?);
    Ok(())
}

/// A campaign store that finds nothing once a plan besides `first` is filed: a
/// delete landing while a revision's jobs stage, however often the campaign is
/// read before then.
struct DeletedOnceFiled {
    inner: Arc<dyn CampaignStore>,
    plans: Arc<dyn PlanStore>,
    first: PlanId,
}

#[async_trait::async_trait]
impl CampaignStore for DeletedOnceFiled {
    async fn get_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<Campaign>> {
        let filed = pipette_store::drain_plans(self.plans.as_ref(), None)
            .await?
            .items
            .iter()
            .any(|plan| plan.plan_id != self.first);
        if filed {
            Ok(None)
        } else {
            self.inner.get_campaign(campaign_id).await
        }
    }
    async fn put_campaign(&self, campaign: &Campaign) -> anyhow::Result<()> {
        self.inner.put_campaign(campaign).await
    }
    async fn list_campaigns(
        &self,
        after: Option<&CampaignId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<CampaignPage> {
        self.inner.list_campaigns(after, limit).await
    }
    async fn delete_campaign(&self, campaign_id: &CampaignId) -> anyhow::Result<()> {
        self.inner.delete_campaign(campaign_id).await
    }
}

/// The server cancels the plan it filed and leaves the head alone, so no plan
/// stays claimable under a deleted campaign.
#[tokio::test]
async fn a_revision_whose_campaign_is_deleted_while_it_files_is_cancelled() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let inner = state.campaign_store.clone();
    state.campaign_store = Arc::new(DeletedOnceFiled {
        inner: inner.clone(),
        plans: state.plan_store.clone(),
        first: first.clone(),
    });

    let resp = post_plan_document(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        ONE_CELL_PLAN,
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;

    let filed: Vec<PlanId> = pipette_store::drain_plans(state.plan_store.as_ref(), None)
        .await?
        .items
        .into_iter()
        .map(|plan| plan.plan_id)
        .filter(|plan_id| *plan_id != first)
        .collect();
    let [filed] = filed.as_slice() else {
        anyhow::bail!("expected the one revision filed, found {filed:?}");
    };
    assert_eq!(
        (status, &body["error"]),
        (
            StatusCode::CONFLICT,
            &json!(format!(
                "campaign {campaign_id} was deleted while plan {filed} was filed into it, so the \
                 plan did not advance; the filing cancelled the plan"
            ))
        )
    );
    assert!(state.plan_store.has_cancel_marker(filed).await?);
    assert!(!state.plan_store.has_cancel_marker(&first).await?);
    let stored = inner
        .get_campaign(&CampaignId::try_new(campaign_id.as_str())?)
        .await?
        .context("the campaign is still in the store")?;
    assert_eq!(stored.current_plan_id, Some(first));
    Ok(())
}

// ── the chain ───────────────────────────────────────────────────────────────

/// A record written before a campaign needed a first plan runs nothing. A
/// revision filed into it supersedes nothing and becomes its head.
#[tokio::test]
async fn a_revision_into_a_record_running_nothing_supersedes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = put_idle_campaign(&state, &owner.1, "Not started").await?;

    let resp = post_plan_document(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plan_id = PlanId::try_new(json_str(&body_json(resp).await?, "plan_id")?)?;

    let manifest = manifest(&state, &plan_id).await?;
    assert_eq!(manifest.campaign_id.as_ref(), Some(&campaign_id));
    assert_eq!(manifest.previous_plan_id, None);
    let stored = state
        .campaign_store
        .get_campaign(&campaign_id)
        .await?
        .context("the campaign is still there")?;
    assert_eq!(stored.current_plan_id, Some(plan_id));
    Ok(())
}

/// ```text
/// Campaign ──current──> P2 ──previous──> P1
/// ```
#[tokio::test]
async fn filing_work_chains_the_new_plan_onto_the_old() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    // The create files the first plan, which supersedes nothing.
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let manifest_one = manifest(&state, &first).await?;
    assert_eq!(
        manifest_one.campaign_id.as_ref().map(|id| id.as_str()),
        Some(campaign_id.as_str()),
        "the plan records the campaign that minted it"
    );
    assert_eq!(
        manifest_one.previous_plan_id, None,
        "the first plan in a campaign supersedes nothing"
    );

    // The revision chains onto it, and the campaign moves.
    let second = filed_plan(&state, &owner, &campaign_id, &client_id, "rev-2").await?;
    let manifest_two = manifest(&state, &second).await?;
    assert_eq!(
        manifest_two.previous_plan_id,
        Some(first.clone()),
        "the new plan names the one it supersedes"
    );
    assert_eq!(
        manifest_two.campaign_id, manifest_one.campaign_id,
        "both belong to the one campaign"
    );

    let (key, user_id, key_id) = &owner;
    let read = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(
        read["current_plan_id"],
        json!(second.as_str()),
        "the campaign runs the revision"
    );

    // And the plan it left behind is untouched: superseding is not cancelling.
    assert_eq!(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/plans/{first}")
        )
        .await?
        .status(),
        StatusCode::OK,
    );
    Ok(())
}

/// A plan document files into a campaign as job bodies do: the plan belongs to
/// the campaign, stores its source, and the campaign moves onto it.
#[tokio::test]
async fn filing_a_plan_document_chains_it_into_the_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let (state, _client_key, _client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;

    let resp = post_plan_document(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/plans?note=rev-1"),
        AFM_MLX,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;
    assert_eq!(report["job_count"], 3);
    assert_eq!(report["note"], "rev-1");
    let plan_id = PlanId::try_new(json_str(&report, "plan_id")?)?;

    let manifest = manifest(&state, &plan_id).await?;
    assert_eq!(
        manifest.campaign_id.as_ref().map(|id| id.as_str()),
        Some(campaign_id.as_str())
    );
    assert!(
        manifest.source_sha256.is_some(),
        "the manifest names no source"
    );

    let (key, user_id, key_id) = &owner;
    let read = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(read["current_plan_id"], json!(plan_id.as_str()));
    Ok(())
}

/// A dry run into a campaign stages nothing and leaves the campaign on the plan
/// it already runs.
#[tokio::test]
async fn a_dry_run_leaves_the_campaign_where_it_is() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let (state, _client_key, _client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;

    let resp = post_plan_document(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/plans?dry_run=true"),
        AFM_MLX,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["bodies"].as_array().map(Vec::len),
        Some(3)
    );

    let (key, user_id, key_id) = &owner;
    let read = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(read["current_plan_id"], json!(first.as_str()));
    // The one job of the first plan, and none from the dry run.
    assert_eq!(state.todo_store.list_avail().await?.len(), 1);
    Ok(())
}

/// Unknown keys are refused, so a leftover `submitter` or a misspelt or
/// malformed `dry_run` answers `400` rather than filing for real. It stages
/// nothing, and the campaign stays on the plan it already runs.
#[rstest]
#[case::submitter("submitter=user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")]
#[case::dry_run_not_a_bool("dry_run=maybe")]
#[case::misspelt_dry_run("dryrun=true")]
#[tokio::test]
async fn an_unknown_key_leaves_the_campaign_where_it_is(#[case] query: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, _client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;

    let uri = format!("/api/v1/campaigns/{campaign_id}/plans?{query}");
    let resp = post_plan_document(&state, &owner, &uri, ONE_CELL_PLAN).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let (key, user_id, key_id) = &owner;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let read = body_json(authed_user_get(&state, key, user_id, key_id, &uri).await?).await?;
    assert_eq!(read["current_plan_id"], json!(first.as_str()));
    // The one job of the first plan, and none from the refused request.
    assert_eq!(state.todo_store.list_avail().await?.len(), 1);
    Ok(())
}

/// Two [`submit_plan_as`] calls under one name stage distinct jobs, and
/// [`put_loose_plan`] leaves a plan outside every campaign, with no
/// predecessor.
#[tokio::test]
async fn the_plan_fixture_files_an_active_plan_into_a_campaign_of_its_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let first = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "twice", 3).await?;
    let second = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "twice", 3).await?;

    let first = manifest(&state, &first).await?;
    let second = manifest(&state, &second).await?;
    for plan in [&first, &second] {
        let campaign_id = plan
            .campaign_id
            .as_ref()
            .context("the plan names its campaign")?;
        let campaign = state
            .campaign_store
            .get_campaign(campaign_id)
            .await?
            .context("the campaign is stored")?;
        assert_eq!(
            campaign.owner,
            CampaignOwner::User {
                user_id: user_id.clone()
            }
        );
        assert_eq!(plan.submitted_by.as_ref(), Some(&user_id));
        assert_eq!(plan.status, PlanStatus::Active);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert!(plan.plan_warnings.is_empty(), "{:?}", plan.plan_warnings);
        assert_eq!(plan.job_ids.len(), 3);
    }
    assert_ne!(first.campaign_id, second.campaign_id);
    assert!(
        first
            .job_ids
            .iter()
            .all(|job_id| !second.job_ids.contains(job_id)),
        "the two plans share a job"
    );

    let campaigns = stored_campaigns(&state).await?;
    let loose = put_loose_plan(&state, &key, &user_id, &key_id, &client_id, "loose", 1).await?;
    let loose = manifest(&state, &loose).await?;
    assert_eq!(loose.campaign_id, None);
    assert_eq!(loose.previous_plan_id, None);
    assert_eq!(loose.submitted_by.as_ref(), Some(&user_id));
    assert_eq!(
        stored_campaigns(&state).await?,
        campaigns,
        "the loose plan's campaign is gone"
    );
    Ok(())
}

/// The gate is the campaign's, not the plan's: holding `submit_plans` files
/// your own work, never into a campaign you do not administer.
#[tokio::test]
async fn filing_into_someone_elses_campaign_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let stranger = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let campaign_id = create_campaign(&state, &owner, "Mine", None).await?;
    let staged = state.todo_store.list_avail().await?.len();

    let resp = file_plan(&state, &stranger, &campaign_id, &client_id, "grab").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        state.todo_store.list_avail().await?.len(),
        staged,
        "the refused revision staged a job"
    );

    // The owner files into it through the same route.
    assert_eq!(
        file_plan(&state, &owner, &campaign_id, &client_id, "ours")
            .await?
            .status(),
        StatusCode::CREATED,
    );
    Ok(())
}

/// A campaign id nobody minted is a `404`, before anything is ingested: a plan
/// must never be staged for a campaign that does not exist.
#[tokio::test]
async fn filing_into_an_unknown_campaign_stages_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let resp = file_plan(&state, &owner, &an_unminted_campaign(), &client_id, "ghost").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// Neither as job bodies nor as a document, though it administers the campaign.
#[tokio::test]
async fn an_org_admin_without_submit_plans_files_no_revision() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let (_admin, team_admin, _acme, campaign_id) = a_team_campaign(&state).await?;
    let first = head(&state, &campaign_id)
        .await?
        .context("the create filed a first plan")?;

    let uri = format!("/api/v1/campaigns/{campaign_id}/plans");
    for (form, resp) in [
        (
            "job bodies",
            file_plan(&state, &team_admin, &campaign_id, &client_id, "rev-2").await?,
        ),
        (
            "a document",
            post_plan_document(&state, &team_admin, &uri, ONE_CELL_PLAN).await?,
        ),
    ] {
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{form}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "user lacks the submit_plans permission",
            "{form}"
        );
    }
    assert_still_on(&state, &campaign_id, &first).await?;

    // It administers the campaign all the same: the refusal is the plan grant.
    assert_eq!(
        rename(&state, &team_admin, &campaign_id, "Release gate v2")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

/// The chain route is cheap and id-only; the filtered listing answers the same
/// question with each plan's status.
#[tokio::test]
async fn a_campaigns_plans_are_readable_as_a_chain_and_as_a_listing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let second = filed_plan(&state, &owner, &campaign_id, &client_id, "rev-2").await?;

    // A plan outside every campaign must appear in neither answer.
    let loose = put_loose_plan(
        &state, &owner.0, &owner.1, &owner.2, &client_id, "one-off", 1,
    )
    .await?;

    let (key, user_id, key_id) = &owner;
    let chain = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}/plans"),
        )
        .await?,
    )
    .await?;
    assert_eq!(chain["current_plan_id"], json!(second.as_str()));
    assert_eq!(chain["superseded_plan_ids"], json!([first.as_str()]));
    assert_eq!(chain["history_truncated"], json!(false));

    let listed = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/plans?campaign_id={campaign_id}"),
        )
        .await?,
    )
    .await?;
    let ids: Vec<&str> = listed["items"]
        .as_array()
        .context("a listing carries an `items` array")?
        .iter()
        .filter_map(|plan| plan["plan_id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2, "the campaign's plans and no others");
    assert!(ids.contains(&second.as_str()) && ids.contains(&first.as_str()));
    assert!(
        !ids.contains(&loose.as_str()),
        "a plan filed outside the campaign is not in it"
    );

    // A plan id is no campaign id, so the filter refuses it rather than matching nothing.
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/plans?campaign_id={first}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // The filter narrows inside the scope rather than beside it: a campaign the
    // reader cannot reach gives an empty listing, not a refusal.
    let stranger = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let (key, user_id, key_id) = &stranger;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/plans?campaign_id={campaign_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await?["items"]
            .as_array()
            .is_some_and(|items| items.is_empty())
    );

    // The chain route answers the campaign's own reach, so the same stranger is
    // refused outright there.
    assert_eq!(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}/plans")
        )
        .await?
        .status(),
        StatusCode::FORBIDDEN,
    );
    Ok(())
}

// ── ownership ───────────────────────────────────────────────────────────────

/// A write in [`a_stranger_cannot_write_someone_elses_campaign`].
#[derive(Clone, Copy, Debug)]
enum OwnerWrite {
    Rename,
    /// A `PATCH` naming nothing, which still answers with the record.
    EmptyPatch,
    /// Hands the campaign to the caller.
    HandOff,
}

/// Parameterized over every write: the gate is per-route, and a route added
/// later is where it would be forgotten.
#[rstest]
#[case::rename(OwnerWrite::Rename)]
#[case::empty_patch(OwnerWrite::EmptyPatch)]
#[case::hand_off(OwnerWrite::HandOff)]
#[tokio::test]
async fn a_stranger_cannot_write_someone_elses_campaign(
    #[case] write: OwnerWrite,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let stranger = operator(&state).await?;

    let campaign_id = create_campaign(&state, &owner, "Mine", None).await?;
    let (method, suffix) = match write {
        OwnerWrite::Rename | OwnerWrite::EmptyPatch => ("PATCH", ""),
        OwnerWrite::HandOff => ("PUT", "/owner"),
    };
    let uri = format!("/api/v1/campaigns/{campaign_id}{suffix}");
    // The hand-off names each caller as the destination, so the only thing
    // separating the two runs is whether the campaign was theirs to begin with.
    let body = |signer: &UserSigner| match write {
        OwnerWrite::Rename => json!({ "name": "Taken over" }),
        OwnerWrite::EmptyPatch => json!({}),
        OwnerWrite::HandOff => json!({ "owner": { "type": "user", "user_id": signer.1 } }),
    };

    let resp = signed_json(&state, &stranger, method, &uri, &body(&stranger)).await?;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "{method} {uri} must refuse an account that does not own the campaign"
    );

    // The owner is admitted through the same route, so the refusal is about
    // ownership and not about the request.
    let resp = signed_json(&state, &owner, method, &uri, &body(&owner)).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// A hand-off naming no owner is a `400` naming `owner`, and the campaign keeps
/// its owner: unlike a device's, a campaign's owner is not nullable.
#[tokio::test]
async fn a_hand_off_naming_no_owner_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Mine", None).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");
    let before = stored_owner(&state, &campaign_id).await?;

    for body in [
        json!({ "owner": null }),
        json!({}),
        json!({ "onwer": { "type": "user", "user_id": owner.1 } }),
    ] {
        let resp = signed_json(&state, &owner, "PUT", &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains("owner"), "{body}: {error}");
        assert_eq!(stored_owner(&state, &campaign_id).await?, before, "{body}");
    }
    Ok(())
}

/// A stranger's delete is refused too, and the record is still there
/// afterwards — a `403` that deleted anyway would be the worst kind of pass.
#[tokio::test]
async fn a_stranger_cannot_delete_someone_elses_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let stranger = operator(&state).await?;
    let (campaign_id, plan_id) = created_campaign(&state, &owner, &[("name", "Mine")]).await?;
    // Stopped, so the owner's delete below is not held by it.
    state.plan_store.write_cancel_marker(&plan_id).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");

    let resp = signed_empty(&state, &stranger, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let (key, user_id, key_id) = &owner;
    assert_eq!(
        authed_user_get(&state, key, user_id, key_id, &uri)
            .await?
            .status(),
        StatusCode::OK,
        "the refused delete must not have written anything"
    );

    let resp = signed_empty(&state, &owner, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let (key, user_id, key_id) = &owner;
    assert_eq!(
        authed_user_get(&state, key, user_id, key_id, &uri)
            .await?
            .status(),
        StatusCode::NOT_FOUND,
    );
    Ok(())
}

/// One campaign write as a test sends it: the method, the URI, and the body
/// under its `Content-Type`, or `None` for a bodiless `DELETE`.
struct CampaignWrite {
    method: &'static str,
    uri: String,
    body: Option<(&'static str, String)>,
}

/// The create, which addresses no campaign yet.
fn the_create() -> CampaignWrite {
    CampaignWrite {
        method: "POST",
        uri: "/api/v1/campaigns?name=Another".to_string(),
        body: Some(("application/toml", ONE_CELL_PLAN.to_string())),
    }
}

/// Every write addressed to `campaign_id`, each with a body its owner `owner`
/// could send: `base` is the plan the campaign runs and `job` a job of it.
fn writes_to(campaign_id: &str, base: &PlanId, job: &str, owner: &UserId) -> Vec<CampaignWrite> {
    let at = |suffix: &str| format!("/api/v1/campaigns/{campaign_id}{suffix}");
    let as_json = |body: Value| Some(("application/json", body.to_string()));
    let as_toml = |body: &str| Some(("application/toml", body.to_string()));
    vec![
        CampaignWrite {
            method: "PATCH",
            uri: at(""),
            body: as_json(json!({ "name": "Renamed" })),
        },
        CampaignWrite {
            method: "DELETE",
            uri: at(""),
            body: None,
        },
        CampaignWrite {
            method: "PUT",
            uri: at("/owner"),
            body: as_json(json!({ "owner": { "type": "user", "user_id": owner } })),
        },
        CampaignWrite {
            method: "POST",
            uri: at("/plans"),
            body: as_toml(ONE_CELL_PLAN),
        },
        CampaignWrite {
            method: "POST",
            uri: at(&format!("/variants?base_plan_id={base}")),
            body: as_toml(MLX_700M_VARIANT),
        },
        CampaignWrite {
            method: "POST",
            uri: at("/rerun"),
            body: as_json(json!({ "job_ids": [job] })),
        },
        CampaignWrite {
            method: "POST",
            uri: at("/recalculate"),
            body: as_json(json!({ "job_ids": [job] })),
        },
    ]
}

/// Send `write` under `headers`, which sign its method and URI.
async fn send_write(
    state: &AppState,
    write: &CampaignWrite,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    match &write.body {
        Some((content_type, body)) => {
            raw_request(
                state,
                write.method,
                &write.uri,
                headers,
                Some(content_type),
                body.clone(),
            )
            .await
        }
        None => request_with_headers(state, write.method, &write.uri, headers).await,
    }
}

/// [`send_write`], signed as `signer`.
async fn send_write_as(
    state: &AppState,
    signer: &UserSigner,
    write: &CampaignWrite,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let (key, user_id, key_id) = signer;
    let headers = user_auth_headers(key, user_id, key_id, write.method, &write.uri)?;
    send_write(state, write, &headers).await
}

/// A write to a campaign nobody minted is a `404` for a caller holding a
/// campaign grant, and a `403` for one holding none: the authority resolves
/// before the lookup, so the second learns nothing about existence. A filing
/// (every `POST` here) is gated on `submit_plans` itself.
#[rstest]
#[case::an_operator(
    &[UserPermission::SubmitPlans],
    StatusCode::NOT_FOUND,
    "no campaign campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    "no campaign campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::no_grant(
    &[],
    StatusCode::FORBIDDEN,
    "campaigns need submit_plans to own one, or manage_orgs to administer your organization's",
    "user lacks the submit_plans permission"
)]
#[tokio::test]
async fn a_write_to_an_unminted_campaign_answers_by_the_callers_authority(
    #[case] grants: &[UserPermission],
    #[case] status: StatusCode,
    #[case] error: &str,
    #[case] filing_error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let signer = registered_user(&state, grants).await?;
    let writes = writes_to(
        &an_unminted_campaign(),
        &a_plan_id(),
        "job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        &signer.1,
    );

    for write in &writes {
        let resp = send_write_as(&state, &signer, write).await?;
        let answered = resp.status();
        let body = body_json(resp).await?;
        assert_eq!(
            (answered, &body["error"]),
            (
                status,
                &json!(if write.method == "POST" {
                    filing_error
                } else {
                    error
                })
            ),
            "{} {}",
            write.method,
            write.uri
        );
    }
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await
}

/// Belonging to the organization is not administering it; its administrator is,
/// another organization's is not, and the server reaches every one. The member,
/// who reads the campaign, is told the grant it lacks.
#[rstest]
#[case::a_member_of_the_owning_org(Actor::Member, StatusCode::FORBIDDEN, Some("needs manage_orgs"))]
#[case::its_own_administrator(Actor::OrgAdmin, StatusCode::OK, None)]
#[case::another_orgs_administrator(Actor::RivalAdmin, StatusCode::FORBIDDEN, Some("another owner"))]
#[case::a_server_admin(Actor::ServerAdmin, StatusCode::OK, None)]
#[tokio::test]
async fn an_org_campaign_answers_the_scoped_authority_rule(
    #[case] actor: Actor,
    #[case] expected: StatusCode,
    #[case] refusal: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let rival = create_org(&state, &admin, "rival").await?;
    let owner = org_admin(&state, &acme).await?;
    let campaign_id = create_campaign(&state, &admin, "Release gate", Some(&acme)).await?;

    let caller = match actor {
        Actor::Member => {
            let member = operator(&state).await?;
            set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
            member
        }
        Actor::OrgAdmin => owner,
        Actor::RivalAdmin => org_admin(&state, &rival).await?,
        Actor::ServerAdmin => admin,
    };

    let resp = rename(&state, &caller, &campaign_id, "Renamed").await?;
    assert_eq!(resp.status(), expected);
    if let Some(refusal) = refusal {
        let error = body_json(resp).await?;
        let error = json_str(&error, "error")?;
        assert!(error.contains(refusal), "{error}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Actor {
    /// On the owning organization, holding no roster grant.
    Member,
    /// The owning organization's administrator.
    OrgAdmin,
    /// Another organization's administrator, which is the line the scope draws.
    RivalAdmin,
    /// `submit_any_plans`, which reaches every campaign.
    ServerAdmin,
}

/// Reading is wider than administering by exactly one step: an ordinary member
/// of the owning organization sees the campaign it may not rename.
#[tokio::test]
async fn a_member_reads_the_campaign_it_may_not_administer() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let member = operator(&state).await?;
    set_user_org(&state, &member.1, Some(org_id.parse()?)).await?;

    let campaign_id = create_campaign(&state, &admin, "Release gate", Some(&org_id)).await?;

    let (key, user_id, key_id) = &member;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{campaign_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["name"], json!("Release gate"));
    Ok(())
}

/// The pointer routes included, so `read_plans` reads no campaign whose id
/// leaked.
#[tokio::test]
async fn a_reader_outside_the_reach_is_refused_every_read_of_the_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let (campaign_id, _) = created_campaign(&state, &owner, &[("name", "Mine")]).await?;
    let stranger = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    for uri in one_campaign_reads(&campaign_id) {
        let (status, body) = get_as(&state, &stranger, &uri).await?;
        assert_eq!(
            (status, &body["error"]),
            (
                StatusCode::FORBIDDEN,
                &json!("this campaign belongs to another owner")
            ),
            "{uri}"
        );
    }

    // The owner reads the same routes, so the refusal is about reach.
    for uri in one_campaign_reads(&campaign_id) {
        let (status, body) = get_as(&state, &owner, &uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    Ok(())
}

/// What an account holding neither campaign read grant is told.
const NO_CAMPAIGN_READ: &str =
    "reading campaigns needs read_plans, or manage_orgs for your organization's";

/// An account holding neither `read_plans` nor `manage_orgs` is refused the
/// listing and every read of one campaign, before the store is asked: an id
/// nobody minted answers the same `403` as a real one.
#[rstest]
#[case::no_grant(&[])]
#[case::read_clients_alone(&[UserPermission::ReadClients])]
#[tokio::test]
async fn an_account_without_a_campaign_read_grant_reads_no_campaign(
    #[case] grants: &[UserPermission],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (campaign_id, _) =
        created_campaign(&state, &operator(&state).await?, &[("name", "Nightly")]).await?;
    let reader = registered_user(&state, grants).await?;

    let uris = std::iter::once("/api/v1/campaigns".to_string())
        .chain(one_campaign_reads(&campaign_id))
        .chain(one_campaign_reads(&an_unminted_campaign()));
    for uri in uris {
        let (status, body) = get_as(&state, &reader, &uri).await?;
        assert_eq!(
            (status, &body["error"]),
            (StatusCode::FORBIDDEN, &json!(NO_CAMPAIGN_READ)),
            "{uri}"
        );
    }
    Ok(())
}

/// A pending account reads no campaign, whatever it holds: seeded with
/// `read_any_plans`, it is refused on its standing on the listing and on every
/// read of one. A route taking the wrong extractor would admit it.
#[tokio::test]
async fn a_pending_account_reads_no_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (campaign_id, _) =
        created_campaign(&state, &operator(&state).await?, &[("name", "Nightly")]).await?;
    let reader = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    set_user_status(&state, &reader.1, UserStatus::Pending).await?;

    let uris =
        std::iter::once("/api/v1/campaigns".to_string()).chain(one_campaign_reads(&campaign_id));
    for uri in uris {
        let (status, body) = get_as(&state, &reader, &uri).await?;
        assert_eq!(
            (status, &body["error"]),
            (
                StatusCode::FORBIDDEN,
                &json!("user is pending an operator's approval")
            ),
            "{uri}"
        );
    }

    // Approved, the same grant reads the campaign: the refusal was the standing.
    set_user_status(&state, &reader.1, UserStatus::Active).await?;
    let (status, body) =
        get_as(&state, &reader, &format!("/api/v1/campaigns/{campaign_id}")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// Who signs the writes in [`a_caller_without_standing_writes_no_campaign`].
#[derive(Clone, Copy, Debug)]
enum Standing {
    /// An approved device, signing with its own key.
    Device,
    /// The campaign's owner, holding every grant a write takes, while pending.
    PendingOwner,
    /// The same owner, disabled.
    DisabledOwner,
}

/// Every campaign write refuses a device at the header, and the campaign's
/// owner holding every grant while it is pending or disabled, and writes
/// nothing. A route taking the wrong extractor would admit one of them.
#[rstest]
#[case::a_device(
    Standing::Device,
    StatusCode::UNAUTHORIZED,
    "invalid X-Client-Id header: user id must start with `user_`"
)]
#[case::a_pending_owner(
    Standing::PendingOwner,
    StatusCode::FORBIDDEN,
    "user is pending an operator's approval"
)]
#[case::a_disabled_owner(Standing::DisabledOwner, StatusCode::FORBIDDEN, "user is disabled")]
#[tokio::test]
async fn a_caller_without_standing_writes_no_campaign(
    #[case] standing: Standing,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, client_key, client_id) = approved_client(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let job = manifest(&state, &first).await?.job_ids[0].clone();
    match standing {
        Standing::Device => {}
        Standing::PendingOwner => set_user_status(&state, &owner.1, UserStatus::Pending).await?,
        Standing::DisabledOwner => set_user_status(&state, &owner.1, UserStatus::Disabled).await?,
    }

    let writes = std::iter::once(the_create()).chain(writes_to(
        &campaign_id,
        &first,
        job.as_str(),
        &owner.1,
    ));
    for write in writes {
        let resp = match standing {
            Standing::Device => {
                let headers = auth_headers(&client_key, &client_id, write.method, &write.uri)?;
                send_write(&state, &write, &headers).await?
            }
            Standing::PendingOwner | Standing::DisabledOwner => {
                send_write_as(&state, &owner, &write).await?
            }
        };
        let answered = resp.status();
        let body = body_json(resp).await?;
        assert_eq!(
            (answered, &body["error"]),
            (status, &json!(error)),
            "{} {}",
            write.method,
            write.uri
        );
    }
    assert_still_on(&state, &campaign_id, &first).await?;
    assert_eq!(stored_campaigns(&state).await?, 1);
    let stored = state
        .campaign_store
        .get_campaign(&CampaignId::try_new(campaign_id.as_str())?)
        .await?
        .context("the campaign is still there")?;
    assert_eq!(stored.name.as_str(), "Nightly");

    // Active, the owner's rename lands: the refusal was the standing.
    set_user_status(&state, &owner.1, UserStatus::Active).await?;
    assert_eq!(
        rename(&state, &owner, &campaign_id, "Renamed")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

/// Either check alone would let a campaign walk out of one scope or into
/// another.
#[tokio::test]
async fn handing_a_campaign_over_is_checked_at_both_ends() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let rival = create_org(&state, &admin, "rival").await?;

    // Owns campaigns, administers `acme`, and has no authority over `rival`.
    let member = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    let campaign_id = create_campaign(&state, &member, "Mine", None).await?;
    let plan_id = filed_plan(&state, &member, &campaign_id, &client_id, "handoff").await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");

    // The far end: a campaign may not be pushed into a roster the caller does
    // not administer.
    let resp = signed_json(
        &state,
        &member,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "org", "org_id": rival } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The near end: somebody else's campaign is not the caller's to hand to
    // itself.
    let stranger = operator(&state).await?;
    let theirs = create_campaign(&state, &stranger, "Theirs", None).await?;
    let resp = signed_json(
        &state,
        &member,
        "PUT",
        &format!("/api/v1/campaigns/{theirs}/owner"),
        &json!({ "owner": { "type": "org", "org_id": acme } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Both ends clear: the hand-off lands, and the campaign keeps what it was
    // running.
    let resp = signed_json(
        &state,
        &member,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "org", "org_id": acme } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], json!(true));
    assert_eq!(body["owner"]["org_id"], json!(acme));
    assert_eq!(
        body["current_plan_id"],
        json!(plan_id.as_str()),
        "a hand-off must not drop what the campaign is running"
    );
    assert!(
        body.get("plan").is_none(),
        "a write answers the record alone: {body}"
    );

    // Handing it where it already is changes nothing and says so, the rule every
    // write here follows.
    let again = signed_json(
        &state,
        &member,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "org", "org_id": acme } }),
    )
    .await?;
    assert_eq!(again.status(), StatusCode::OK);
    let again = body_json(again).await?;
    assert_eq!(again["changed"], json!(false));
    assert_eq!(
        again["updated_at"], body["updated_at"],
        "a no-op must not move the timestamp"
    );

    // And the previous owner has given it up: it holds `submit_plans`, but the
    // campaign is the organization's now, and it administers that organization.
    // A colleague who does not is the one that proves the transfer took.
    let colleague = operator(&state).await?;
    set_user_org(&state, &colleague.1, Some(acme.parse()?)).await?;
    assert_eq!(
        rename(&state, &colleague, &campaign_id, "Mine now")
            .await?
            .status(),
        StatusCode::FORBIDDEN,
        "an ordinary member of the new owner still may not administer it"
    );
    Ok(())
}

/// A campaign owned by `acme`, its administrator, and the server admin that
/// created it: the opening both hand-off tests share.
async fn a_team_campaign(
    state: &AppState,
) -> anyhow::Result<(UserSigner, UserSigner, String, String)> {
    let admin = server_admin(state).await?;
    let acme = create_org(state, &admin, "acme").await?;
    let team_admin = org_admin(state, &acme).await?;
    let campaign_id = create_campaign(state, &admin, "Release gate", Some(&acme)).await?;
    Ok((admin, team_admin, acme, campaign_id))
}

/// Both directions are pinned: a hand-down only a server admin could undo would
/// be worse than none.
#[tokio::test]
async fn an_org_admin_hands_a_campaign_to_a_member_and_back() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (_admin, team_admin, acme, campaign_id) = a_team_campaign(&state).await?;
    let member = operator(&state).await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");

    // Down to the member, by the organization's administrator.
    let resp = signed_json(
        &state,
        &team_admin,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "user", "user_id": member.1 } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["owner"]["user_id"],
        json!(member.1.as_str())
    );

    // It is the member's now, and not the administrator's to take back.
    assert_eq!(
        rename(&state, &team_admin, &campaign_id, "Mine again")
            .await?
            .status(),
        StatusCode::FORBIDDEN,
        "handing it down gives it up: a personal campaign is personal"
    );

    // But the member may return it, which is what keeps the hand-down from
    // being one-way.
    let resp = signed_json(
        &state,
        &member,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "org", "org_id": acme } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rename(&state, &team_admin, &campaign_id, "Release gate v2")
            .await?
            .status(),
        StatusCode::OK,
        "and its administrator reaches it again"
    );
    Ok(())
}

#[tokio::test]
async fn owning_a_campaign_without_submit_plans_names_the_missing_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &owner.1, Some(acme.parse()?)).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    // `manage_orgs` on `acme` keeps a campaign authority, so the refusal is
    // the per-campaign one and not the no-authority `403`.
    grant_user(&state, &owner.1, &[UserPermission::ManageOrgs]).await?;

    let resp = rename(&state, &owner, &campaign_id, "Mine now").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "administering your own campaigns needs submit_plans"
    );
    let (status, read) =
        get_as(&state, &owner, &format!("/api/v1/campaigns/{campaign_id}")).await?;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["name"], json!("Nightly"), "a refused rename wrote");
    assert_eq!(read["owner"]["user_id"], json!(owner.1.as_str()));

    // Granted it again, the same rename lands: the refusal was the grant.
    grant_user(
        &state,
        &owner.1,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    assert_eq!(
        rename(&state, &owner, &campaign_id, "Mine now")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

/// The roster is the boundary: an administrator may not hand a campaign to
/// somebody outside the organization it administers, nor to an account on none.
#[tokio::test]
async fn a_hand_off_stops_at_the_roster_edge() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (admin, team_admin, _acme, campaign_id) = a_team_campaign(&state).await?;
    let rival = create_org(&state, &admin, "rival").await?;

    let outsider = operator(&state).await?;
    set_user_org(&state, &outsider.1, Some(rival.parse()?)).await?;
    let unaffiliated = operator(&state).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");

    for (who, target) in [
        ("another organization's member", outsider.1.clone()),
        ("an account on no organization", unaffiliated.1.clone()),
    ] {
        let resp = signed_json(
            &state,
            &team_admin,
            "PUT",
            &uri,
            &json!({ "owner": { "type": "user", "user_id": target } }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "must refuse {who}");
    }

    // An account that does not exist reads the same, so a refused hand-off does
    // not report whether the destination is real.
    let resp = signed_json(
        &state,
        &team_admin,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "user", "user_id": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e" } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The server admin, who may hand it anywhere, gets the honest `400`.
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "user", "user_id": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e" } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// `PUT /api/v1/campaigns/{campaign_id}/owner` as `signer`, handing it to `owner`:
/// the status alone.
async fn hand_over(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    owner: Value,
) -> anyhow::Result<StatusCode> {
    Ok(signed_json(
        state,
        signer,
        "PUT",
        &format!("/api/v1/campaigns/{campaign_id}/owner"),
        &json!({ "owner": owner }),
    )
    .await?
    .status())
}

/// The owner `campaign_id` is stored under.
async fn stored_owner(
    state: &AppState,
    campaign_id: &str,
) -> anyhow::Result<pipette_core::campaign::CampaignOwner> {
    Ok(state
        .campaign_store
        .get_campaign(&pipette_plan_types::CampaignId::try_new(campaign_id)?)
        .await?
        .context("the campaign exists")?
        .owner)
}

/// An org admin puts a campaign down only on its own organization or its
/// members, and takes none from another; either refusal leaves the campaign
/// where it was.
#[tokio::test]
async fn only_a_server_admin_moves_a_campaign_between_organizations() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (admin, team_admin, acme, campaign_id) = a_team_campaign(&state).await?;
    let rival = create_org(&state, &admin, "rival").await?;
    let rival_admin = org_admin(&state, &rival).await?;
    let to_rival = json!({ "type": "org", "org_id": rival });

    // Out of its own organization: the far end refuses.
    assert_eq!(
        hand_over(&state, &team_admin, &campaign_id, to_rival.clone()).await?,
        StatusCode::FORBIDDEN
    );
    // Into its own from another: the near end refuses.
    assert_eq!(
        hand_over(&state, &rival_admin, &campaign_id, to_rival.clone()).await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        stored_owner(&state, &campaign_id)
            .await?
            .org_id()
            .map(ToString::to_string),
        Some(acme.clone()),
        "a refused move leaves the campaign where it was"
    );

    // The server admin moves it, and its administration moves with it.
    assert_eq!(
        hand_over(&state, &admin, &campaign_id, to_rival).await?,
        StatusCode::OK
    );
    assert_eq!(
        stored_owner(&state, &campaign_id)
            .await?
            .org_id()
            .map(ToString::to_string),
        Some(rival.clone())
    );
    assert_eq!(
        rename(&state, &team_admin, &campaign_id, "Not ours")
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rename(&state, &rival_admin, &campaign_id, "Ours now")
            .await?
            .status(),
        StatusCode::OK
    );

    // And the org admin who received it cannot send it back either.
    assert_eq!(
        hand_over(
            &state,
            &rival_admin,
            &campaign_id,
            json!({ "type": "org", "org_id": acme })
        )
        .await?,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// Not to another organization, and not to a colleague: only the organization's
/// administrator hands campaigns to members.
#[tokio::test]
async fn a_member_gives_its_campaign_only_to_its_own_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let rival = create_org(&state, &admin, "rival").await?;
    let member = operator(&state).await?;
    let colleague = operator(&state).await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    set_user_org(&state, &colleague.1, Some(acme.parse()?)).await?;
    let first = create_campaign(&state, &member, "First", None).await?;

    for (to, owner) in [
        (
            "another organization",
            json!({ "type": "org", "org_id": rival }),
        ),
        (
            "a colleague",
            json!({ "type": "user", "user_id": colleague.1 }),
        ),
    ] {
        assert_eq!(
            hand_over(&state, &member, &first, owner).await?,
            StatusCode::FORBIDDEN,
            "{to}"
        );
    }
    assert_eq!(
        stored_owner(&state, &first).await?.user_id(),
        Some(&member.1),
        "a refused hand-off leaves it the member's"
    );
    assert_eq!(
        hand_over(
            &state,
            &member,
            &first,
            json!({ "type": "org", "org_id": acme })
        )
        .await?,
        StatusCode::OK
    );

    // Moved to rival, the account is rival's alone: its next campaign goes to
    // rival, and no longer to acme.
    set_user_org(&state, &member.1, Some(rival.parse()?)).await?;
    assert_eq!(
        state
            .org_store
            .get_membership(&member.1)
            .await?
            .map(|org_id| org_id.to_string()),
        Some(rival.clone())
    );
    let memberships: Vec<_> = state
        .org_store
        .list_memberships()
        .await?
        .into_iter()
        .filter(|membership| membership.user_id == member.1)
        .collect();
    assert_eq!(memberships.len(), 1, "one membership, not two");

    let second = create_campaign(&state, &member, "Second", None).await?;
    assert_eq!(
        hand_over(
            &state,
            &member,
            &second,
            json!({ "type": "org", "org_id": acme })
        )
        .await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        hand_over(
            &state,
            &member,
            &second,
            json!({ "type": "org", "org_id": rival })
        )
        .await?,
        StatusCode::OK
    );
    Ok(())
}

/// Handing a campaign to an account without `submit_plans` is refused, the
/// administrator itself included, and the campaign stays put until the grant.
#[tokio::test]
async fn a_hand_off_needs_an_owner_holding_submit_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (_admin, team_admin, acme, campaign_id) = a_team_campaign(&state).await?;
    let member = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");

    for target in [&member.1, &team_admin.1] {
        let resp = signed_json(
            &state,
            &team_admin,
            "PUT",
            &uri,
            &json!({ "owner": { "type": "user", "user_id": target } }),
        )
        .await?;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "handing it to {target}"
        );
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains("owner needs submit_plans"), "{error}");
    }
    let (key, user_id, key_id) = &member;
    let read = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{campaign_id}"),
    )
    .await?;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(
        body_json(read).await?["owner"]["org_id"],
        json!(acme),
        "a refused hand-off leaves the campaign with its owner"
    );

    grant_user(
        &state,
        &member.1,
        &[UserPermission::ReadPlans, UserPermission::SubmitPlans],
    )
    .await?;
    let resp = signed_json(
        &state,
        &team_admin,
        "PUT",
        &uri,
        &json!({ "owner": { "type": "user", "user_id": member.1 } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["owner"]["user_id"],
        json!(member.1.as_str())
    );
    Ok(())
}

/// What `GET /api/v1/plans/{plan_id}` answers `signer` for each of `plans`.
async fn plan_statuses(
    state: &AppState,
    signer: &UserSigner,
    plans: &[&PlanId],
) -> anyhow::Result<Vec<StatusCode>> {
    let (key, user_id, key_id) = signer;
    futures::future::try_join_all(plans.iter().map(|plan_id| async move {
        let uri = format!("/api/v1/plans/{plan_id}");
        anyhow::Ok(
            authed_user_get(state, key, user_id, key_id, &uri)
                .await?
                .status(),
        )
    }))
    .await
}

/// `POST /api/v1/plans/{plan_id}/cancel` as `signer`: the status, and any `error`.
async fn cancel_plan(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &PlanId,
) -> anyhow::Result<(StatusCode, String)> {
    let resp = signed_empty(
        state,
        signer,
        "POST",
        &format!("/api/v1/plans/{plan_id}/cancel"),
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok((status, error.to_string()))
}

/// An author's personal campaign with two plans filed, the first superseded; an
/// ordinary member of its organization and the organization's administrator.
/// All three hold `submit_plans` and `cancel_plans`.
struct TeamHandOff {
    state: AppState,
    acme: String,
    author: UserSigner,
    member: UserSigner,
    team_admin: UserSigner,
    campaign_id: String,
    first: PlanId,
    second: PlanId,
    _dir: tempfile::TempDir,
}

impl TeamHandOff {
    async fn new() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let (state, _client_key, client_id) = approved_client(dir.path()).await?;
        let admin = server_admin(&state).await?;
        let acme = create_org(&state, &admin, "acme").await?;
        let grants = [UserPermission::SubmitPlans, UserPermission::CancelPlans];
        let author = registered_user(&state, &grants).await?;
        let member = registered_user(&state, &grants).await?;
        let team_admin = registered_user(
            &state,
            &[&grants[..], &[UserPermission::ManageOrgs]].concat(),
        )
        .await?;
        let org_id: pipette_plan_types::OrgId = acme.parse()?;
        futures::future::try_join_all(
            [&author, &member, &team_admin]
                .map(|signer| set_user_org(&state, &signer.1, Some(org_id.clone()))),
        )
        .await?;
        let (campaign_id, first) =
            created_campaign(&state, &author, &[("name", "Nightly")]).await?;
        let second = filed_plan(&state, &author, &campaign_id, &client_id, "nightly").await?;
        Ok(Self {
            state,
            acme,
            author,
            member,
            team_admin,
            campaign_id,
            first,
            second,
            _dir: dir,
        })
    }

    /// Hand the campaign to `owner` as `signer`, asserting it landed.
    async fn hand_to(&self, signer: &UserSigner, owner: Value) -> anyhow::Result<()> {
        let resp = signed_json(
            &self.state,
            signer,
            "PUT",
            &format!("/api/v1/campaigns/{}/owner", self.campaign_id),
            &json!({ "owner": owner }),
        )
        .await?;
        anyhow::ensure!(
            resp.status() == StatusCode::OK,
            "the hand-off failed with {}",
            resp.status()
        );
        Ok(())
    }
}

/// Handing a campaign to a team hands over every plan it filed, the superseded
/// one included: members read and cancel them, administrators delete the
/// campaign, and no plan record is rewritten.
#[tokio::test]
async fn handing_a_campaign_to_the_team_hands_over_its_plans() -> anyhow::Result<()> {
    let team = TeamHandOff::new().await?;
    let state = &team.state;
    let plans = [&team.first, &team.second];
    let job_id = manifest(state, &team.first)
        .await?
        .job_ids
        .first()
        .cloned()
        .context("the first plan holds a job")?;
    let by_job = format!("/api/v1/jobs/{job_id}/plans");
    let (key, user_id, key_id) = &team.member;

    assert_eq!(
        plan_statuses(state, &team.member, &plans).await?,
        [StatusCode::NOT_FOUND; 2],
        "a personal campaign's plans are its owner's alone"
    );
    assert_eq!(
        authed_user_get(state, key, user_id, key_id, &by_job)
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let (status, error) = cancel_plan(state, &team.member, &team.second).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(error.contains("cancel_any_plans"), "{error}");

    team.hand_to(&team.author, json!({ "type": "org", "org_id": team.acme }))
        .await?;

    assert_eq!(
        plan_statuses(state, &team.member, &plans).await?,
        [StatusCode::OK; 2]
    );
    let listing = listed_plans(state, &team.member, "/api/v1/plans").await?;
    assert!(
        plans
            .iter()
            .all(|plan_id| listing.contains(&plan_id.to_string())),
        "{listing:?}"
    );
    let resp = authed_user_get(state, key, user_id, key_id, &by_job).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["plans"][0]["plan_id"],
        json!(team.first.as_str())
    );

    assert_eq!(
        plan_statuses(state, &team.author, &plans).await?,
        [StatusCode::OK; 2],
        "the author reads them as any member does"
    );

    // The team's plans are each member's to cancel, the author's included.
    for (signer, plan_id) in [(&team.member, &team.first), (&team.author, &team.second)] {
        let (status, error) = cancel_plan(state, signer, plan_id).await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    }
    // The record is the administrators'.
    let delete = format!("/api/v1/campaigns/{}", team.campaign_id);
    let resp = signed_empty(state, &team.member, "DELETE", &delete).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = signed_empty(state, &team.team_admin, "DELETE", &delete).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    for plan_id in plans {
        let stored = manifest(state, plan_id).await?;
        assert_eq!(stored.submitted_by.as_ref(), Some(&team.author.1));
        assert_eq!(
            stored.campaign_id.map(|id| id.to_string()),
            Some(team.campaign_id.clone()),
            "the plan still names the campaign that filed it"
        );
    }
    Ok(())
}

/// `cancel_plans` reaches the team's plans on membership alone, from a member
/// who files nothing, and reaches none of them from outside the organization,
/// on no organization or on another.
#[tokio::test]
async fn a_member_holding_cancel_plans_alone_cancels_the_team_s_plan() -> anyhow::Result<()> {
    let team = TeamHandOff::new().await?;
    let state = &team.state;
    team.hand_to(&team.author, json!({ "type": "org", "org_id": team.acme }))
        .await?;
    let outsider = registered_user(state, &[UserPermission::CancelPlans]).await?;
    let rival = registered_user(state, &[UserPermission::CancelPlans]).await?;
    let other = create_org(state, &server_admin(state).await?, "rival").await?;
    set_user_org(state, &rival.1, Some(other.parse()?)).await?;
    let canceller = registered_user(state, &[UserPermission::CancelPlans]).await?;
    set_user_org(state, &canceller.1, Some(team.acme.parse()?)).await?;

    for signer in [&outsider, &rival] {
        let (status, error) = cancel_plan(state, signer, &team.second).await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(error.contains("cancel_any_plans"), "{error}");
    }
    let (status, error) = cancel_plan(state, &canceller, &team.second).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    Ok(())
}

/// A disabled member's personal campaign is its organization's administrator's
/// to wind down: it reads the record, cancels the plans and hands it over. It
/// reads no plan and files nothing before that. An active member's campaign
/// stays personal, and nobody else gains anything.
#[tokio::test]
async fn an_org_admin_winds_down_a_disabled_member_s_campaign() -> anyhow::Result<()> {
    let team = TeamHandOff::new().await?;
    let state = &team.state;
    let record = format!("/api/v1/campaigns/{}", team.campaign_id);
    let plans = [&team.first, &team.second];
    let reads = async |signer: &UserSigner| -> anyhow::Result<StatusCode> {
        Ok(signed_empty(state, signer, "GET", &record).await?.status())
    };
    let to_acme = json!({ "owner": { "type": "org", "org_id": team.acme } });
    let hands_over = async |signer: &UserSigner| -> anyhow::Result<StatusCode> {
        let uri = format!("{record}/owner");
        Ok(signed_json(state, signer, "PUT", &uri, &to_acme)
            .await?
            .status())
    };

    // While its owner is active, it is personal.
    assert_eq!(reads(&team.team_admin).await?, StatusCode::FORBIDDEN);
    assert_eq!(hands_over(&team.team_admin).await?, StatusCode::FORBIDDEN);
    let (status, error) = cancel_plan(state, &team.team_admin, &team.second).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(error.contains("cancel_any_plans"), "{error}");

    set_user_status(state, &team.author.1, UserStatus::Disabled).await?;

    // The administrator reads the record, listed with its organization's.
    let overview = signed_empty(
        state,
        &team.team_admin,
        "GET",
        &format!("{record}/overview"),
    )
    .await?;
    assert_eq!(overview.status(), StatusCode::OK);
    assert_eq!(body_json(overview).await?["may_administer"], json!(true));
    // The app's hand-over field asks where it may go.
    let owners = signed_empty(
        state,
        &team.team_admin,
        "GET",
        &format!("{record}/owners?q="),
    )
    .await?;
    assert_eq!(owners.status(), StatusCode::OK);
    let listing = signed_empty(state, &team.team_admin, "GET", "/api/v1/campaigns").await?;
    let listing = body_json(listing).await?;
    let listed = listing["items"]
        .as_array()
        .context("the listing has items")?
        .iter()
        .any(|row| row["campaign_id"] == json!(team.campaign_id));
    assert!(listed, "{listing}");

    // Not its plans, and it files nothing into it.
    assert_eq!(
        plan_statuses(state, &team.team_admin, &plans).await?,
        [StatusCode::NOT_FOUND; 2]
    );
    let document = signed_empty(
        state,
        &team.team_admin,
        "GET",
        &format!("{record}/document"),
    )
    .await?;
    assert_eq!(document.status(), StatusCode::FORBIDDEN);

    // A member gains nothing, nor does another organization's administrator.
    let rival = create_org(state, &server_admin(state).await?, "rival").await?;
    let rival_admin = org_admin(state, &rival).await?;
    for signer in [&team.member, &rival_admin] {
        assert_eq!(reads(signer).await?, StatusCode::FORBIDDEN);
        assert_eq!(hands_over(signer).await?, StatusCode::FORBIDDEN);
    }

    for plan_id in plans {
        let (status, error) = cancel_plan(state, &team.team_admin, plan_id).await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    }
    assert_eq!(hands_over(&team.team_admin).await?, StatusCode::OK);
    assert_eq!(
        plan_statuses(state, &team.team_admin, &plans).await?,
        [StatusCode::OK; 2],
        "handed over, its plans are the organization's"
    );
    Ok(())
}

/// The wind-down stops at rank, as account administration does: a disabled
/// member holding a server-wide name the administrator lacks keeps its
/// campaign.
#[tokio::test]
async fn an_org_admin_winds_down_no_campaign_of_an_account_that_outranks_it() -> anyhow::Result<()>
{
    let team = TeamHandOff::new().await?;
    let state = &team.state;
    state
        .user_store
        .mutate_user_permissions(&team.author.1, &|set| {
            set.insert_all([UserPermission::ReadAnyPlans])
        })
        .await?;
    set_user_status(state, &team.author.1, UserStatus::Disabled).await?;

    let record = format!("/api/v1/campaigns/{}", team.campaign_id);
    let resp = signed_empty(state, &team.team_admin, "GET", &record).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let (status, _) = cancel_plan(state, &team.team_admin, &team.second).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    Ok(())
}

/// Handing the campaign on hands its plans on with it: once the team's
/// administrator hands it down to a member, it is that member's personal
/// campaign, and nobody else on the team reads its plans.
#[tokio::test]
async fn handing_a_campaign_on_takes_its_plans_with_it() -> anyhow::Result<()> {
    let team = TeamHandOff::new().await?;
    let state = &team.state;
    let plans = [&team.first, &team.second];
    team.hand_to(&team.author, json!({ "type": "org", "org_id": team.acme }))
        .await?;
    team.hand_to(
        &team.team_admin,
        json!({ "type": "user", "user_id": team.member.1 }),
    )
    .await?;

    for signer in [&team.author, &team.team_admin] {
        assert_eq!(
            plan_statuses(state, signer, &plans).await?,
            [StatusCode::NOT_FOUND; 2]
        );
    }
    assert_eq!(
        plan_statuses(state, &team.member, &plans).await?,
        [StatusCode::OK; 2]
    );
    let (status, error) = cancel_plan(state, &team.member, &team.second).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    Ok(())
}

/// `scope=mine` is the plans of the caller's campaigns and its organization's,
/// whoever filed them: a `read_any_plans` holder narrowing to it keeps a
/// colleague's plans in the team's campaign and drops a stranger's.
#[tokio::test]
async fn narrowing_to_mine_keeps_the_team_s_campaigns() -> anyhow::Result<()> {
    let team = TeamHandOff::new().await?;
    let state = &team.state;
    team.hand_to(&team.author, json!({ "type": "org", "org_id": team.acme }))
        .await?;
    let (_, stranger_plan) =
        created_campaign(state, &operator(state).await?, &[("name", "Elsewhere")]).await?;
    grant_user(
        state,
        &team.member.1,
        &[UserPermission::SubmitPlans, UserPermission::ReadAnyPlans],
    )
    .await?;

    let mine = listed_plans(state, &team.member, "/api/v1/plans?scope=mine").await?;
    assert!(
        [&team.first, &team.second]
            .iter()
            .all(|plan_id| mine.contains(&plan_id.to_string())),
        "{mine:?}"
    );
    assert!(!mine.contains(&stranger_plan.to_string()), "{mine:?}");
    let every = listed_plans(state, &team.member, "/api/v1/plans").await?;
    assert!(every.contains(&stranger_plan.to_string()), "{every:?}");
    Ok(())
}

/// Another owner's campaign's plans are read by `read_any_plans` and
/// cancelled by `cancel_any_plans`: `submit_any_plans` reaches neither, and
/// `manage_any_orgs` administers the campaign without them.
#[tokio::test]
async fn only_the_any_plan_names_reach_another_campaign_s_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, _client_id) = approved_client(dir.path()).await?;
    let (campaign_id, plan_id) =
        created_campaign(&state, &operator(&state).await?, &[("name", "Nightly")]).await?;
    let plan = plan_id.to_string();
    let record = format!("/api/v1/campaigns/{campaign_id}");

    for (grants, reads_record) in [
        (
            &[UserPermission::SubmitAnyPlans, UserPermission::CancelPlans][..],
            StatusCode::FORBIDDEN,
        ),
        (
            &[
                UserPermission::ManageAnyOrgs,
                UserPermission::ReadPlans,
                UserPermission::CancelPlans,
            ][..],
            StatusCode::OK,
        ),
    ] {
        let outsider = registered_user(&state, grants).await?;
        // An administrator reads back the record it may repoint, not its plans.
        let resp = signed_empty(&state, &outsider, "GET", &record).await?;
        assert_eq!(resp.status(), reads_record, "{grants:?}");
        assert!(
            !listed_plans(&state, &outsider, "/api/v1/plans")
                .await?
                .contains(&plan),
            "{grants:?}"
        );
        let (status, error) = cancel_plan(&state, &outsider, &plan_id).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{grants:?}: {error}");
    }

    let reader = registered_user(
        &state,
        &[UserPermission::ReadAnyPlans, UserPermission::CancelAnyPlans],
    )
    .await?;
    assert_eq!(
        plan_statuses(&state, &reader, &[&plan_id]).await?,
        [StatusCode::OK]
    );
    assert!(
        !listed_plans(&state, &reader, "/api/v1/plans?scope=mine")
            .await?
            .contains(&plan)
    );
    let (status, error) = cancel_plan(&state, &reader, &plan_id).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    Ok(())
}

/// The route that files a revision in [`a_revision_is_attributed_to_whoever_files_it`].
#[derive(Clone, Copy, Debug)]
enum RevisionRoute {
    Plans,
    Variants,
    Document,
    Rerun,
    Recalculate,
}

/// Who files that revision into a campaign another account created.
#[derive(Clone, Copy, Debug)]
enum Reviser {
    /// The administrator of the organization that owns the campaign.
    TeamAdmin,
    /// A member of that organization, holding no roster grant.
    TeamMember,
    /// A `manage_any_orgs` delegate, over a campaign an operator owns.
    Delegate,
}

/// On every route that files one, while the head keeps its own filer: never the
/// campaign's owner or the head's `submitted_by`.
#[rstest]
#[tokio::test]
async fn a_revision_is_attributed_to_whoever_files_it(
    #[values(
        RevisionRoute::Plans,
        RevisionRoute::Variants,
        RevisionRoute::Document,
        RevisionRoute::Rerun,
        RevisionRoute::Recalculate
    )]
    route: RevisionRoute,
    #[values(Reviser::TeamAdmin, Reviser::TeamMember, Reviser::Delegate)] reviser: Reviser,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let editing = [
        pipette_web_testkit::FULL_EDITOR,
        &[UserPermission::ReadPlans],
    ]
    .concat();
    let author = registered_user(&state, &editing).await?;
    let (campaign_id, filer) = match reviser {
        Reviser::TeamAdmin | Reviser::TeamMember => {
            let admin = server_admin(&state).await?;
            let acme = create_org(&state, &admin, "acme").await?;
            let roster: &[UserPermission] = if matches!(reviser, Reviser::TeamAdmin) {
                &[UserPermission::ManageOrgs]
            } else {
                &[]
            };
            let colleague = registered_user(&state, &[&editing[..], roster].concat()).await?;
            for member in [&author, &colleague] {
                set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
            }
            let campaign_id = create_campaign(&state, &author, "Team", Some(&acme)).await?;
            (campaign_id, colleague)
        }
        Reviser::Delegate => {
            let delegate = registered_user(
                &state,
                &[
                    &editing[..],
                    &[
                        UserPermission::ManageAnyOrgs,
                        UserPermission::ReadAnyPlans,
                        UserPermission::CancelAnyPlans,
                    ],
                ]
                .concat(),
            )
            .await?;
            (
                create_campaign(&state, &author, "Mine", None).await?,
                delegate,
            )
        }
    };
    let first = head(&state, &campaign_id)
        .await?
        .context("the create filed a first plan")?;

    let base = format!("?base_plan_id={first}");
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    let named = json!({ "job_ids": [done.as_str()] });
    let revised = match route {
        RevisionRoute::Plans => None,
        RevisionRoute::Variants => {
            Some(edit_variants(&state, &filer, &campaign_id, &base, MLX_700M_VARIANT).await?)
        }
        RevisionRoute::Document => {
            let widened = ONE_CELL_PLAN.replace(
                "[\"prefill_throughput_256\"]",
                "[\"prefill_throughput_256\", \"max_memory_usage_256\"]",
            );
            Some(revise_document(&state, &filer, &campaign_id, &base, &widened).await?)
        }
        RevisionRoute::Rerun => {
            pipette_store_testkit::finish_job(
                state.submission_store.as_ref(),
                state.todo_store.as_ref(),
                &done,
            )
            .await?;
            Some(rerun(&state, &filer, &campaign_id, &named).await?)
        }
        RevisionRoute::Recalculate => {
            answer(&state, &done, "eval").await?;
            Some(recalculate(&state, &filer, &campaign_id, "", &named).await?)
        }
    };
    let revision = match revised {
        None => filed_plan(&state, &filer, &campaign_id, &client_id, "revision").await?,
        Some(resp) => {
            let status = resp.status();
            let body = body_json(resp).await?;
            assert_eq!(status, StatusCode::CREATED, "{route:?}: {body}");
            PlanId::try_new(json_str(&body["plan"], "plan_id")?)?
        }
    };

    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    assert_eq!(
        manifest(&state, &revision).await?.submitted_by.as_ref(),
        Some(&filer.1),
        "the revision records its filer"
    );
    assert_eq!(
        manifest(&state, &first).await?.submitted_by.as_ref(),
        Some(&author.1),
        "the head keeps its own filer"
    );
    Ok(())
}

// ── the pointer ─────────────────────────────────────────────────────────────

/// No route points a campaign at a plan of the caller's choosing: the pointer
/// moves by filing work and by nothing else.
#[tokio::test]
async fn the_id_and_the_pointer_survive_the_writes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let plan_id = filed_plan(&state, &owner, &campaign_id, &client_id, "survives").await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");

    assert_eq!(
        rename(&state, &owner, &campaign_id, "Nightly LFM2")
            .await?
            .status(),
        StatusCode::OK,
    );

    let (key, user_id, key_id) = &owner;
    let read = body_json(authed_user_get(&state, key, user_id, key_id, &uri).await?).await?;
    assert_eq!(
        read["campaign_id"],
        json!(campaign_id),
        "the id never moves"
    );
    assert_eq!(read["name"], json!("Nightly LFM2"));
    assert_eq!(
        read["current_plan_id"],
        json!(plan_id.as_str()),
        "a rename must not disturb what the campaign is running"
    );

    // No route offers to point it at a plan of the caller's choosing.
    let resp = signed_json(
        &state,
        &owner,
        "PUT",
        &format!("/api/v1/campaigns/{campaign_id}/plan"),
        &json!({ "plan_id": PlanId::from_uuid(uuid::Uuid::now_v7()).as_str() }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    Ok(())
}

/// That makes a retry after a lost response safe rather than a second edit.
#[tokio::test]
async fn a_write_to_the_current_value_is_a_successful_no_op() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;

    let first = body_json(rename(&state, &owner, &campaign_id, "Nightly LFM2").await?).await?;
    assert_eq!(first["changed"], json!(true));

    let again = body_json(rename(&state, &owner, &campaign_id, "Nightly LFM2").await?).await?;
    assert_eq!(again["changed"], json!(false));
    assert_eq!(
        again["updated_at"], first["updated_at"],
        "a no-op must not move the timestamp"
    );
    Ok(())
}

/// A rename takes `name` and nothing else, so a misspelt field, or one that
/// belongs to another route, is a `400` and the record is left as it was.
#[tokio::test]
async fn a_rename_refuses_a_field_it_does_not_take() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let (key, user_id, key_id) = &owner;
    let before = body_json(authed_user_get(&state, key, user_id, key_id, &uri).await?).await?;
    let caller = json!({ "type": "user", "user_id": user_id.as_str() });

    for (body, field) in [
        (json!({ "nmae": "Renamed" }), "nmae"),
        (json!({ "name": "Renamed", "owner": caller }), "owner"),
        (
            json!({ "name": "Renamed", "current_plan_id": a_plan_id().as_str() }),
            "current_plan_id",
        ),
    ] {
        let resp = signed_json(&state, &owner, "PATCH", &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains(field), "the refusal names {field}: {error}");
        let read = body_json(authed_user_get(&state, key, user_id, key_id, &uri).await?).await?;
        assert_eq!(read, before, "a refused rename writes nothing: {body}");
    }
    Ok(())
}

/// Answered `changed: false`, with `updated_at` and the record's fields
/// untouched.
#[tokio::test]
async fn a_patch_naming_no_name_moves_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let (campaign_id, _) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let (status, before) = get_as(&state, &owner, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{before}");

    let resp = signed_json(&state, &owner, "PATCH", &uri, &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let answered = body_json(resp).await?;
    assert_eq!(answered["changed"], json!(false), "{answered}");
    for field in ["name", "owner", "current_plan_id", "updated_at"] {
        assert_eq!(answered[field], before[field], "{field}");
    }

    let (status, after) = get_as(&state, &owner, &uri).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after, before, "nothing was written");

    // The no-op answers with the record, so it keeps the write gate.
    let stranger = operator(&state).await?;
    let nobody = registered_user(&state, &[]).await?;
    for (signer, error) in [
        (&stranger, "this campaign belongs to another owner"),
        (&nobody, "campaigns need submit_plans"),
    ] {
        let resp = signed_json(&state, signer, "PATCH", &uri, &json!({})).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{error}");
        let message = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(message.starts_with(error), "{message}");
    }
    Ok(())
}

/// A rename is held to the create's bounds on a name: a blank one and one past
/// 4096 characters are refused and the record keeps its name; 4096 itself
/// lands.
#[rstest]
#[case::blank("   ".to_string(), StatusCode::BAD_REQUEST, "non-empty string")]
#[case::past_the_ceiling(
    "x".repeat(4097),
    StatusCode::BAD_REQUEST,
    "name must not exceed 4096 characters"
)]
#[case::at_the_ceiling("x".repeat(4096), StatusCode::OK, "")]
#[tokio::test]
async fn a_rename_is_held_to_the_name_bounds(
    #[case] name: String,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let (read, before) = get_as(&state, &owner, &uri).await?;
    assert_eq!(read, StatusCode::OK, "{before}");

    let resp = rename(&state, &owner, &campaign_id, &name).await?;
    assert_eq!(resp.status(), status);
    let answered = body_json(resp).await?;
    let (read, after) = get_as(&state, &owner, &uri).await?;
    assert_eq!(read, StatusCode::OK, "{after}");
    if status == StatusCode::OK {
        assert_eq!(answered["changed"], json!(true), "{answered}");
        assert_eq!(after["name"], json!(name));
    } else {
        let message = json_str(&answered, "error")?;
        assert!(message.contains(error), "{message}");
        assert_eq!(after["name"], json!("Nightly"));
        assert_eq!(after, before, "a refused rename wrote the record");
    }
    Ok(())
}

/// A `PATCH` with `labels` sets the whole set: the record and its listing row
/// answer it, the same set again is a no-op, a new set drops the keys it does
/// not name, and an empty one leaves the record unlabelled.
#[tokio::test]
async fn labels_are_set_whole_and_read_back() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let patch = |body: Value| patch_campaign(&state, &owner, &uri, body);
    let (_, unlabelled) = get_as(&state, &owner, &uri).await?;
    assert!(unlabelled.get("labels").is_none(), "{unlabelled}");

    let labels = json!({ "study": "acmelm-text", "Device": "S26 Ultra", "kind": "" });
    let set = patch(json!({ "labels": labels })).await?;
    assert_eq!((&set["changed"], &set["labels"]), (&json!(true), &labels));
    assert_eq!(set["name"], json!("Nightly"), "labels alone leave the name");
    let (_, read) = get_as(&state, &owner, &uri).await?;
    assert_eq!(read["labels"], labels);
    let (_, overview) = get_as(&state, &owner, &format!("{uri}/overview")).await?;
    assert_eq!(overview["labels"], labels, "{overview}");
    let (_, listed) = get_as(&state, &owner, "/api/v1/campaigns").await?;
    assert_eq!(listed.pointer("/items/0/labels"), Some(&labels), "{listed}");

    let again = patch(json!({ "labels": labels })).await?;
    assert_eq!(again["changed"], json!(false));
    assert_eq!(again["updated_at"], set["updated_at"]);

    let both = patch(json!({ "name": "Nightly v2", "labels": { "study": "acmelm" } })).await?;
    assert_eq!(both["name"], json!("Nightly v2"));
    assert_eq!(both["labels"], json!({ "study": "acmelm" }));

    let cleared = patch(json!({ "labels": {} })).await?;
    assert_eq!(cleared["changed"], json!(true));
    assert!(cleared.get("labels").is_none(), "{cleared}");
    let renamed = patch(json!({ "name": "Nightly v3" })).await?;
    assert!(renamed.get("labels").is_none(), "a rename sets no labels");
    Ok(())
}

/// A `PATCH` with `state` archives the campaign and unarchives it: the record,
/// its overview and its listing row answer `archived`, the same state again is
/// a no-op, and an unarchived record carries no `state`. An unknown state is a
/// `400`.
#[tokio::test]
async fn a_campaign_is_archived_and_unarchived() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let patch = |body: Value| patch_campaign(&state, &owner, &uri, body);
    let (_, active) = get_as(&state, &owner, &uri).await?;
    assert!(active.get("state").is_none(), "{active}");

    let archived = patch(json!({ "state": "archived" })).await?;
    assert_eq!(
        (&archived["changed"], &archived["state"]),
        (&json!(true), &json!("archived"))
    );
    assert_eq!(
        archived["name"],
        json!("Nightly"),
        "a state alone leaves the name"
    );
    let (_, read) = get_as(&state, &owner, &uri).await?;
    assert_eq!(read["state"], json!("archived"));
    let (_, overview) = get_as(&state, &owner, &format!("{uri}/overview")).await?;
    assert_eq!(overview["state"], json!("archived"), "{overview}");
    let (_, listed) = get_as(&state, &owner, "/api/v1/campaigns").await?;
    assert_eq!(
        listed.pointer("/items/0/state"),
        Some(&json!("archived")),
        "{listed}"
    );
    for (query, expected) in [
        ("?state=archived", vec![campaign_id.clone()]),
        ("?state=active", vec![]),
        ("?state=", vec![campaign_id.clone()]),
    ] {
        let (_, listed) = get_as(&state, &owner, &format!("/api/v1/campaigns{query}")).await?;
        assert_eq!(campaign_ids(&listed)?, expected, "{query}");
    }
    let (status, _) = get_as(&state, &owner, "/api/v1/campaigns?state=retired").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let again = patch(json!({ "state": "archived" })).await?;
    assert_eq!(again["changed"], json!(false));
    assert_eq!(again["updated_at"], archived["updated_at"]);

    let unarchived = patch(json!({ "state": "active" })).await?;
    assert_eq!(unarchived["changed"], json!(true));
    assert!(unarchived.get("state").is_none(), "{unarchived}");

    let resp = signed_json(
        &state,
        &owner,
        "PATCH",
        &uri,
        &json!({ "state": "retired" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// An archived campaign refuses every filing with a `409` and files nothing, and
/// its overview offers no revision. Unarchived, a filing lands again.
#[tokio::test]
async fn an_archived_campaign_files_no_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let job = manifest(&state, &first).await?.job_ids[0].clone();
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let overview = format!("{uri}/overview");
    patch_campaign(&state, &owner, &uri, json!({ "state": "archived" })).await?;

    let filings: Vec<_> = writes_to(&campaign_id, &first, job.as_str(), &owner.1)
        .into_iter()
        .filter(|write| write.method == "POST")
        .collect();
    for write in &filings {
        let resp = send_write_as(&state, &owner, write).await?;
        let answered = resp.status();
        let body = body_json(resp).await?;
        assert_eq!(
            answered,
            StatusCode::CONFLICT,
            "{} {}",
            write.method,
            write.uri
        );
        let error = json_str(&body, "error")?;
        assert!(error.contains("is archived"), "{error}");
    }
    assert_still_on(&state, &campaign_id, &first).await?;
    let (_, archived) = get_as(&state, &owner, &overview).await?;
    assert_eq!(
        (&archived["may_edit_variants"], &archived["may_rerun"]),
        (&json!(false), &json!(false)),
        "{archived}"
    );

    patch_campaign(&state, &owner, &uri, json!({ "state": "active" })).await?;
    let (_, active) = get_as(&state, &owner, &overview).await?;
    assert_eq!(active["may_edit_variants"], json!(true), "{active}");
    let file_plan = filings
        .iter()
        .find(|write| write.uri.ends_with("/plans"))
        .context("writes_to files a plan")?;
    let resp = send_write_as(&state, &owner, file_plan).await?;
    assert!(resp.status().is_success(), "{}", resp.status());
    Ok(())
}

/// Labels outside the rules are a `400` naming the key, and the record keeps
/// the labels it had.
#[rstest]
#[case::a_character(json!({ "a,b": "x" }), "a,b")]
#[case::a_padded_value(json!({ "study": " acmelm" }), "must not start or end with a space")]
#[case::an_empty_key(json!({ "": "x" }), "must not be empty")]
#[case::a_long_key(json!({ "k".repeat(129): "x" }), "too long")]
#[case::a_value_that_is_no_string(json!({ "study": 3 }), "string")]
#[case::too_many(
    Value::Object((0..51).map(|i| (format!("k{i}"), json!(""))).collect()),
    "at most 50 labels"
)]
#[tokio::test]
async fn labels_outside_the_rules_are_refused(
    #[case] labels: Value,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let kept = json!({ "labels": { "study": "acmelm-text" } });
    let resp = signed_json(&state, &owner, "PATCH", &uri, &kept).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, before) = get_as(&state, &owner, &uri).await?;

    let resp = signed_json(&state, &owner, "PATCH", &uri, &json!({ "labels": labels })).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    let (_, after) = get_as(&state, &owner, &uri).await?;
    assert_eq!(after, before, "a refused write changed the record");
    Ok(())
}

/// A record written before a campaign needed a first plan points at nothing,
/// and says so in its own words rather than answering the plan route with an
/// empty body.
#[tokio::test]
async fn a_campaign_pointing_at_nothing_resolves_to_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = put_idle_campaign(&state, &owner.1, "Not started").await?;

    let (key, user_id, key_id) = &owner;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{campaign_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await?["current_plan_id"].is_null(),
        "the field is absent on a record that runs nothing"
    );

    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{campaign_id}/plan"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// The chain of a record that runs nothing is empty rather than a `404`, where
/// its pointer is one: the chain is what the campaign filed, and that is nothing.
#[tokio::test]
async fn a_campaign_running_nothing_has_an_empty_chain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let campaign_id = put_idle_campaign(&state, &owner.1, "Not started").await?;

    let (status, body) = get_as(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "current_plan_id": null, "superseded_plan_ids": [], "history_truncated": false })
    );
    Ok(())
}

/// The pointer answers what `GET /api/v1/plans/{plan_id}` answers: a colleague
/// holding `read_plans` resolves it to a plan somebody else filed, and a reader
/// holding `manage_orgs` alone is refused the plan.
#[tokio::test]
async fn resolving_the_pointer_answers_what_the_plan_route_answers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;

    let author = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &author.1, Some(org_id.parse()?)).await?;
    let campaign_id = create_campaign(&state, &author, "Team nightly", Some(&org_id)).await?;
    let plan_id = filed_plan(&state, &author, &campaign_id, &client_id, "team-nightly").await?;

    let colleague = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    set_user_org(&state, &colleague.1, Some(org_id.parse()?)).await?;
    let (key, user_id, key_id) = &colleague;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{campaign_id}/plan"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanView", &body)?;
    assert_eq!(
        body["plan_id"],
        json!(plan_id.as_str()),
        "the campaign resolves to the plan that is executing"
    );

    let team_admin = org_admin(&state, &org_id).await?;
    let (key, user_id, key_id) = &team_admin;
    assert_eq!(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/campaigns/{campaign_id}")
        )
        .await?
        .status(),
        StatusCode::OK,
        "the pointer is the team's to see"
    );
    for uri in [
        format!("/api/v1/campaigns/{campaign_id}/plan"),
        format!("/api/v1/plans/{plan_id}"),
    ] {
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
        let message = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(message.contains("read_plans"), "{uri}: {message}");
    }
    Ok(())
}

/// The pointer takes the plan route's `names=true`, and answers the plan
/// route's body either way: the submitter by address when asked, else none.
#[tokio::test]
async fn the_pointer_names_the_submitter_as_the_plan_route_does() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let plan_id = filed_plan(&state, &owner, &campaign_id, &client_id, "named").await?;
    let address = user_address(&state, &owner.1).await?;

    for query in ["", "?names=true"] {
        let pointer = format!("/api/v1/campaigns/{campaign_id}/plan{query}");
        let (status, body) = get_as(&state, &owner, &pointer).await?;
        assert_eq!(status, StatusCode::OK, "{pointer}: {body}");
        crate::api_schema::assert_conforms("NamedPlanView", &body)?;
        let (_, direct) =
            get_as(&state, &owner, &format!("/api/v1/plans/{plan_id}{query}")).await?;
        assert_eq!(body, direct, "{pointer}");
        let names = body.get("names").cloned();
        let expected = (!query.is_empty()).then(|| json!({ owner.1.as_str(): address }));
        assert_eq!(names, expected, "{pointer}");
    }
    let (status, body) = get_as(
        &state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/plan?names=maybe"),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

/// Under `read_plans` each read through the pointer is the plan route's `404`;
/// `read_any_plans` resolves it.
#[rstest]
#[case::own_scope(UserPermission::ReadPlans, false)]
#[case::every_plan(UserPermission::ReadAnyPlans, true)]
#[tokio::test]
async fn a_pointer_at_a_plan_the_campaign_did_not_file_keeps_the_reader_s_scope(
    #[case] grant: UserPermission,
    #[case] resolves: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let reader = registered_user(&state, &[grant]).await?;

    // Somebody else's plan, filed outside every campaign, under a campaign of
    // the reader's that points at it: a state no write produces.
    let (other_key, other_id, other_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let foreign = put_loose_plan(
        &state,
        &other_key,
        &other_id,
        &other_key_id,
        &client_id,
        "foreign",
        1,
    )
    .await?;
    let campaign_id = put_idle_campaign(&state, &reader.1, "Pointer").await?;
    let mut campaign = state
        .campaign_store
        .get_campaign(&campaign_id)
        .await?
        .context("the campaign is stored")?;
    campaign.current_plan_id = Some(foreign.clone());
    state.campaign_store.put_campaign(&campaign).await?;

    let (status, direct) = get_as(&state, &reader, &format!("/api/v1/plans/{foreign}")).await?;
    if resolves {
        assert_eq!(status, StatusCode::OK, "{direct}");
        let uri = format!("/api/v1/campaigns/{campaign_id}/plan");
        let (status, body) = get_as(&state, &reader, &uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        crate::api_schema::assert_conforms("NamedPlanView", &body)?;
        assert_eq!(body["plan_id"], json!(foreign.as_str()), "{uri}");
    } else {
        assert_eq!(
            (status, &direct),
            (StatusCode::NOT_FOUND, &json!({ "error": "plan not found" }))
        );
        let uri = format!("/api/v1/campaigns/{campaign_id}/plan");
        let (status, body) = get_as(&state, &reader, &uri).await?;
        assert_eq!((status, &body), (StatusCode::NOT_FOUND, &direct), "{uri}");
    }
    Ok(())
}

/// The chain is ids, not plans, so it is not plan-scope-checked: an
/// organization's administrator holding `manage_orgs` alone reads the team
/// campaign's chain, though the pointer refuses it the plan.
#[tokio::test]
async fn an_org_admin_without_read_plans_reads_the_team_campaign_s_chain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let (admin, team_admin, _acme, campaign_id) = a_team_campaign(&state).await?;
    let first = head(&state, &campaign_id)
        .await?
        .context("the create filed a first plan")?;
    let second = filed_plan(&state, &admin, &campaign_id, &client_id, "rev-2").await?;

    let (status, body) = get_as(
        &state,
        &team_admin,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "current_plan_id": second.as_str(),
            "superseded_plan_ids": [first.as_str()],
            "history_truncated": false,
        })
    );

    let (status, body) = get_as(
        &state,
        &team_admin,
        &format!("/api/v1/campaigns/{campaign_id}/plan"),
    )
    .await?;
    assert_eq!(
        (status, &body["error"]),
        (
            StatusCode::FORBIDDEN,
            &json!("user lacks the read_plans permission")
        )
    );
    Ok(())
}

/// Refused while any plan the campaign filed runs, superseded ones included;
/// once all are cancelled the delete goes through and the plans outlive it.
#[tokio::test]
async fn deleting_a_campaign_waits_for_its_plans_and_keeps_them() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let author = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::CancelPlans,
        ],
    )
    .await?;
    let (campaign_id, first) = created_campaign(&state, &author, &[("name", "Nightly")]).await?;
    let second = filed_plan(&state, &author, &campaign_id, &client_id, "nightly").await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let cancel = |plan_id: &PlanId| format!("/api/v1/plans/{plan_id}/cancel");

    let resp = signed_empty(&state, &author, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(
        error.contains(first.as_str()) && error.contains(second.as_str()),
        "{error}"
    );

    let resp = signed_empty(&state, &author, "POST", &cancel(&second)).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let resp = signed_empty(&state, &author, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(
        error.contains(first.as_str()) && !error.contains(second.as_str()),
        "only the superseded plan still runs: {error}"
    );

    let resp = signed_empty(&state, &author, "POST", &cancel(&first)).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let resp = signed_empty(&state, &author, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Nobody reaches a deleted campaign, not even `submit_any_plans`, so only
    // `read_any_plans` reads its plans, listed or one at a time.
    let everywhere = registered_user(&state, &[UserPermission::SubmitAnyPlans]).await?;
    let any_reader = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    for (signer, status) in [
        (&author, StatusCode::NOT_FOUND),
        (&everywhere, StatusCode::NOT_FOUND),
        (&any_reader, StatusCode::OK),
    ] {
        let user_id = &signer.1;
        assert_eq!(
            plan_statuses(&state, signer, &[&first, &second]).await?,
            [status; 2],
            "as {user_id}"
        );
        let listing = listed_plans(&state, signer, "/api/v1/plans").await?;
        assert_eq!(
            listing.contains(&first.to_string()) && listing.contains(&second.to_string()),
            status == StatusCode::OK,
            "the listing as {user_id} agrees with the status route: {listing:?}"
        );
    }
    Ok(())
}

/// A deleted campaign's plan is nobody's to administer: the author who
/// cancelled it with `cancel_plans` while the campaign stood is told it now
/// needs `cancel_any_plans`, and a holder of that grant cancels it.
#[tokio::test]
async fn a_deleted_campaign_s_plan_takes_cancel_any_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let author = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let (campaign_id, plan_id) = created_campaign(&state, &author, &[("name", "Nightly")]).await?;

    // Its own campaign's plan, so `cancel_plans` stops it and the delete lands.
    let (status, error) = cancel_plan(&state, &author, &plan_id).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    let resp = signed_empty(
        &state,
        &author,
        "DELETE",
        &format!("/api/v1/campaigns/{campaign_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (status, error) = cancel_plan(&state, &author, &plan_id).await?;
    assert_eq!(
        (status, error.as_str()),
        (
            StatusCode::FORBIDDEN,
            "user lacks the cancel_any_plans permission"
        )
    );
    let any_canceller = registered_user(&state, &[UserPermission::CancelAnyPlans]).await?;
    let (status, error) = cancel_plan(&state, &any_canceller, &plan_id).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{error}");
    Ok(())
}

// ── listing ─────────────────────────────────────────────────────────────────

/// The listing is the reach: an account sees its own campaigns and its
/// organization's, and nobody else's. The `owner` filter narrows inside that,
/// and never widens it.
#[tokio::test]
async fn the_listing_shows_the_callers_reach() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;

    let member = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &member.1, Some(org_id.parse()?)).await?;
    let stranger = operator(&state).await?;

    let mine = create_campaign(&state, &member, "Mine", None).await?;
    let team = create_campaign(&state, &member, "Team", Some(&org_id)).await?;
    let theirs = create_campaign(&state, &stranger, "Theirs", None).await?;

    let (key, user_id, key_id) = &member;
    let body = body_json(authed_user_get(&state, key, user_id, key_id, "/api/v1/campaigns").await?)
        .await?;
    let mut seen = campaign_ids(&body)?;
    seen.sort();
    let mut expected = vec![mine.clone(), team.clone()];
    expected.sort();
    assert_eq!(seen, expected, "a stranger's campaign is not in reach");
    assert!(!seen.contains(&theirs));

    // The two filters inside the reach.
    let body = body_json(
        authed_user_get(&state, key, user_id, key_id, "/api/v1/campaigns?owner=mine").await?,
    )
    .await?;
    assert_eq!(campaign_ids(&body)?, vec![mine]);
    let body = body_json(
        authed_user_get(&state, key, user_id, key_id, "/api/v1/campaigns?owner=org").await?,
    )
    .await?;
    assert_eq!(campaign_ids(&body)?, vec![team]);

    // The server admin sees every one of them.
    let (key, user_id, key_id) = &admin;
    let body = body_json(authed_user_get(&state, key, user_id, key_id, "/api/v1/campaigns").await?)
        .await?;
    assert_eq!(campaign_ids(&body)?.len(), 3);
    Ok(())
}

/// `org_id` on its own narrows the listing to that organization's campaigns,
/// dropping the caller's personal ones, and naming an organization outside the
/// reach is an empty page rather than a refusal or a widening.
#[tokio::test]
async fn the_org_id_filter_narrows_inside_the_reach() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let rival = create_org(&state, &admin, "rival").await?;
    let member = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;

    // A personal campaign the filter must drop, and one the reach must hide.
    create_campaign(&state, &member, "Mine", None).await?;
    let team = create_campaign(&state, &member, "Team", Some(&acme)).await?;
    create_campaign(&state, &admin, "Rival", Some(&rival)).await?;

    let (status, body) =
        get_as(&state, &member, &format!("/api/v1/campaigns?org_id={acme}")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(campaign_ids(&body)?, vec![team]);

    let (status, body) = get_as(
        &state,
        &member,
        &format!("/api/v1/campaigns?org_id={rival}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "items": [], "next_cursor": null, "unreadable": 0 })
    );
    Ok(())
}

/// The listing pages by cursor: walking `next_cursor` one row at a time yields
/// the caller's campaigns in key order, each once, and never a stranger's.
#[tokio::test]
async fn the_listing_pages_through_a_mixed_keyspace() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let caller = operator(&state).await?;
    let stranger = operator(&state).await?;

    // Created in turn, and ids are UUIDv7, so the stranger's keys sit between
    // the caller's and a page has to read past one to fill.
    let mut expected = Vec::new();
    let mut theirs = Vec::new();
    for round in ["one", "two", "three"] {
        expected.push(create_campaign(&state, &caller, round, None).await?);
        theirs.push(create_campaign(&state, &stranger, round, None).await?);
    }
    expected.sort();
    let (Some(first), Some(last)) = (expected.first(), expected.last()) else {
        anyhow::bail!("the caller created no campaign");
    };
    assert!(
        theirs.iter().any(|id| first < id && id < last),
        "no stranger key inside the caller's range: {expected:?} {theirs:?}"
    );

    let mut seen = Vec::new();
    let mut uri = "/api/v1/campaigns?limit=1".to_string();
    for page in 1.. {
        anyhow::ensure!(page <= 10, "no null cursor after {seen:?}");
        let (status, body) = get_as(&state, &caller, &uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let ids = campaign_ids(&body)?;
        assert!(ids.len() <= 1, "{uri}: {body}");
        assert_eq!(body["unreadable"], json!(0), "{uri}");
        seen.extend(ids);
        let Some(cursor) = body["next_cursor"].as_str() else {
            assert!(body["next_cursor"].is_null(), "{uri}: {body}");
            break;
        };
        uri = format!("/api/v1/campaigns?limit=1&cursor={cursor}");
    }
    assert_eq!(seen, expected);

    // A well-formed cursor past every key is the end of the listing.
    let (status, body) = get_as(&state, &caller, "/api/v1/campaigns?cursor=campaign_zzzz").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "items": [], "next_cursor": null, "unreadable": 0 })
    );
    Ok(())
}

/// `order=newest` walks the same campaigns from the newest down, the vault
/// listing's order, since the two share their query; an unknown order is a `400`.
#[tokio::test]
async fn the_listing_pages_newest_first_when_asked() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let caller = operator(&state).await?;
    let stranger = operator(&state).await?;
    let mut expected = Vec::new();
    for round in ["one", "two", "three"] {
        expected.push(create_campaign(&state, &caller, round, None).await?);
        create_campaign(&state, &stranger, round, None).await?;
    }
    expected.sort();
    expected.reverse();

    let mut seen = Vec::new();
    let mut uri = "/api/v1/campaigns?order=newest&limit=2".to_string();
    for page in 1.. {
        anyhow::ensure!(page <= 10, "no null cursor after {seen:?}");
        let (status, body) = get_as(&state, &caller, &uri).await?;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        seen.extend(campaign_ids(&body)?);
        let Some(cursor) = body["next_cursor"].as_str() else {
            break;
        };
        uri = format!("/api/v1/campaigns?order=newest&limit=2&cursor={cursor}");
    }
    assert_eq!(seen, expected);

    let (status, _) = get_as(&state, &caller, "/api/v1/campaigns?order=bogus").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

/// A campaign record on disk that does not parse is counted in `unreadable`
/// and takes no row, so one bad object does not take the listing down.
#[tokio::test]
async fn an_unreadable_campaign_is_counted_not_fatal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let mine = create_campaign(&state, &owner, "Mine", None).await?;
    std::fs::write(
        dir.path()
            .join("campaigns")
            .join(format!("{}.json", an_unminted_campaign())),
        b"{ this is not a record",
    )?;

    let (status, body) = get_as(&state, &owner, "/api/v1/campaigns").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(campaign_ids(&body)?, vec![mine]);
    assert_eq!(body["unreadable"], json!(1));
    Ok(())
}

// ── request shapes ──────────────────────────────────────────────────────────

/// An organization id nobody minted.
const GHOST_ORG: &str = "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

/// Who sends a create in [`a_refused_create_files_nothing`].
#[derive(Clone, Copy, Debug)]
enum Caller {
    /// Holds `submit_plans` and administers nobody.
    Operator,
    /// Reaches every owner, so only an owner's existence can refuse it.
    ServerAdmin,
}

/// A create refused before anything is filed: by the query string's shape, by
/// a bound on a name, or by the owner, which a dry run is held to as well. A
/// misspelt key in particular must not file for real.
#[rstest]
#[case::blank_name(Caller::Operator, vec![("name", "   ".into())], StatusCode::BAD_REQUEST, "")]
#[case::no_name(Caller::Operator, vec![], StatusCode::BAD_REQUEST, "name")]
#[case::owner_not_a_principal(
    Caller::Operator,
    vec![("name", "Nightly".into()), ("owner", "acme".into())],
    StatusCode::BAD_REQUEST,
    ""
)]
#[case::misspelt_dry_run(
    Caller::Operator,
    vec![("name", "Nightly".into()), ("dry-run", "true".into())],
    StatusCode::BAD_REQUEST,
    "dry-run"
)]
#[case::name_past_the_ceiling(
    Caller::Operator,
    vec![("name", "x".repeat(4097))],
    StatusCode::BAD_REQUEST,
    "4096"
)]
#[case::note_past_the_ceiling(
    Caller::Operator,
    vec![("name", "Nightly".into()), ("note", "x".repeat(4097))],
    StatusCode::BAD_REQUEST,
    "note"
)]
#[case::owner_that_does_not_exist(
    Caller::ServerAdmin,
    vec![("name", "Ghost".into()), ("owner", GHOST_ORG.into())],
    StatusCode::BAD_REQUEST,
    "no organization"
)]
#[case::dry_run_for_an_owner_that_does_not_exist(
    Caller::ServerAdmin,
    vec![("name", "Ghost".into()), ("owner", GHOST_ORG.into()), ("dry_run", "true".into())],
    StatusCode::BAD_REQUEST,
    "no organization"
)]
#[case::dry_run_for_an_owner_out_of_scope(
    Caller::Operator,
    vec![("name", "Theirs".into()), ("owner", GHOST_ORG.into()), ("dry_run", "true".into())],
    StatusCode::FORBIDDEN,
    "you may only create campaigns"
)]
#[tokio::test]
async fn a_refused_create_files_nothing(
    #[case] caller: Caller,
    #[case] params: Vec<(&'static str, String)>,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let signer = match caller {
        Caller::Operator => operator(&state).await?,
        Caller::ServerAdmin => server_admin(&state).await?,
    };
    let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let resp = post_campaign(&state, &signer, &params).await?;
    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await
}

/// The refusals a malformed request gets, each distinct from the others: an id
/// that is not a campaign id never reaches the store, and a JSON create is not
/// a plan document.
#[tokio::test]
async fn a_malformed_request_is_refused_at_the_boundary() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = operator(&state).await?;
    let (key, user_id, key_id) = &owner;

    // A plan id is not a campaign id: the prefix is what makes this a `400`
    // rather than a lookup that finds nothing.
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{}", a_plan_id()),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A well-formed id nobody minted is a `404`.
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        "/api/v1/campaigns/campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // A JSON body is not a plan document: there is no empty create.
    let resp = signed_json(
        &state,
        &owner,
        "POST",
        "/api/v1/campaigns?name=Nightly",
        &json!({ "name": "Nightly" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    assert_eq!(stored_campaigns(&state).await?, 0);
    assert_nothing_staged(&state).await?;
    Ok(())
}

// ── variant edits ───────────────────────────────────────────────────────────

/// An account that may edit the variants of its own campaign
/// ([`pipette_web_testkit::FULL_EDITOR`]).
async fn editor(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, pipette_web_testkit::FULL_EDITOR).await
}

/// Post `edit` to `POST /api/v1/campaigns/{campaign_id}/variants` as `signer`.
async fn edit_variants(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    query: &str,
    edit: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    post_plan_document(
        state,
        signer,
        &format!("/api/v1/campaigns/{campaign_id}/variants{query}"),
        edit,
    )
    .await
}

/// The plan `campaign_id` runs, read from the store.
async fn head(state: &AppState, campaign_id: &str) -> anyhow::Result<Option<PlanId>> {
    Ok(state
        .campaign_store
        .get_campaign(&CampaignId::try_new(campaign_id)?)
        .await?
        .context("the campaign exists")?
        .current_plan_id)
}

/// A variant edit files a revision holding the job it kept and the one it
/// added, moves the campaign onto it, and cancels the plan it replaced. A
/// second edit, made against the revision, drops the first cell: the plan it
/// replaces loses that job, and the added one carries on.
#[tokio::test]
async fn a_variant_edit_keeps_what_it_kept_and_cancels_what_it_replaced() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let first_job = manifest(&state, &first).await?.job_ids;

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("?note=rev-1&base_plan_id={first}"),
        MLX_700M_VARIANT,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["previous_plan_id"], json!(first.as_str()));
    assert_eq!(body["carried_job_count"], 1);
    assert_eq!(body["dropped_job_count"], 0);
    assert_eq!(body["cancel_requested"], true);
    assert_eq!(body["plan"]["job_count"], 2);
    assert_eq!(body["plan"]["note"], "rev-1");
    assert_eq!(
        body["plan"]["change"],
        json!({"type": "variants", "removed_variant_count": 0, "added_variant_count": 1})
    );
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;

    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    let revised = manifest(&state, &revision).await?;
    assert_eq!(revised.previous_plan_id, Some(first.clone()));
    assert!(
        revised.job_ids.contains(&first_job[0]),
        "the kept job carried"
    );
    assert!(state.plan_store.has_cancel_marker(&first).await?);
    assert_eq!(state.todo_store.list_avail().await?.len(), 2);

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={revision}"),
        "remove = [0]",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["previous_plan_id"], json!(revision.as_str()));
    assert_eq!(body["carried_job_count"], 1);
    assert_eq!(body["dropped_job_count"], 1);
    let latest = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    let kept = manifest(&state, &latest).await?.job_ids;
    assert_eq!(kept.len(), 1);
    assert!(!kept.contains(&first_job[0]), "the dropped job is not held");
    Ok(())
}

/// A dry run answers the document the edit would file and its bodies, and
/// files nothing: the campaign stays on its plan, which is not cancelled.
#[tokio::test]
async fn a_dry_run_edit_answers_the_document_and_files_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}&dry_run=true"),
        MLX_700M_VARIANT,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "document")?.contains("LFM2-700M-MLX-4bit"),
        "{body}"
    );
    assert_eq!(body["bodies"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["carried_job_count"], 1);
    assert_eq!(body["new_job_count"], 1);
    assert_eq!(body["dropped_job_ids"], json!([]));

    assert_eq!(head(&state, &campaign_id).await?, Some(first.clone()));
    assert!(!state.plan_store.has_cancel_marker(&first).await?);
    assert_eq!(state.todo_store.list_avail().await?.len(), 1);
    Ok(())
}

/// A `[[variants]]` block a variant edit takes as it is and the planner
/// refuses: it names no runtime.
const VARIANT_WITHOUT_A_RUNTIME: &str = r#"
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]
"#;

/// Who sends an edit in [`a_refused_edit_files_nothing`].
#[derive(Clone, Copy, Debug)]
enum Editor {
    Owner,
    /// Files plans of its own, and does not administer this campaign.
    Stranger,
}

/// An edit refused before anything is filed: by the query string, by the edit
/// itself or the planner on the document it revises to, by a campaign that
/// moved off the plan the caller read, or by the caller. The campaign stays on
/// its plan, which is not cancelled. `{head}` in a query stands for the
/// campaign's plan.
#[rstest]
#[case::misspelt_dry_run(
    Editor::Owner,
    "?base_plan_id={head}&dry-run=true",
    "remove = [0]",
    StatusCode::BAD_REQUEST,
    "dry-run"
)]
#[case::no_base(
    Editor::Owner,
    "",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "base_plan_id"
)]
#[case::no_change(
    Editor::Owner,
    "?base_plan_id={head}",
    "",
    StatusCode::BAD_REQUEST,
    "neither adds nor removes"
)]
#[case::no_such_variant(
    Editor::Owner,
    "?base_plan_id={head}",
    "remove = [1]",
    StatusCode::BAD_REQUEST,
    "variant 1 does not exist"
)]
#[case::everything_removed(
    Editor::Owner,
    "?base_plan_id={head}",
    "remove = [0]",
    StatusCode::BAD_REQUEST,
    "keeps at least one"
)]
#[case::not_an_edit(
    Editor::Owner,
    "?base_plan_id={head}",
    "benchmarks = []",
    StatusCode::BAD_REQUEST,
    "not valid"
)]
#[case::a_block_the_planner_refuses(
    Editor::Owner,
    "?base_plan_id={head}",
    VARIANT_WITHOUT_A_RUNTIME,
    StatusCode::BAD_REQUEST,
    "missing field `runtimes`"
)]
#[case::a_block_the_planner_refuses_dry_run(
    Editor::Owner,
    "?base_plan_id={head}&dry_run=true",
    VARIANT_WITHOUT_A_RUNTIME,
    StatusCode::BAD_REQUEST,
    "missing field `runtimes`"
)]
#[case::a_base_that_is_no_plan_id(
    Editor::Owner,
    "?base_plan_id=bad!id",
    "remove = [0]",
    StatusCode::BAD_REQUEST,
    "plan_id"
)]
#[case::inherit_from_no_plan_id(
    Editor::Owner,
    "?base_plan_id={head}&inherit_from=bad!id",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "plan_id"
)]
#[case::inherit_from_a_missing_plan(
    Editor::Owner,
    "?base_plan_id={head}&inherit_from=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    MLX_700M_VARIANT,
    StatusCode::NOT_FOUND,
    "no plan plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::add_from_no_plan_id(
    Editor::Owner,
    "?base_plan_id={head}&add_from=bad!id",
    "",
    StatusCode::BAD_REQUEST,
    "plan_id"
)]
#[case::add_from_a_missing_plan(
    Editor::Owner,
    "?base_plan_id={head}&add_from=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    "",
    StatusCode::NOT_FOUND,
    "no plan plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::restore_no_index(
    Editor::Owner,
    "?base_plan_id={head}&restore={head}",
    "",
    StatusCode::BAD_REQUEST,
    "has no `:`"
)]
#[case::restore_a_missing_plan(
    Editor::Owner,
    "?base_plan_id={head}&restore=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e:0",
    "",
    StatusCode::NOT_FOUND,
    "no plan plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::restore_a_missing_variant(
    Editor::Owner,
    "?base_plan_id={head}&restore={head}:1",
    "",
    StatusCode::BAD_REQUEST,
    "has 1 variant(s), so it has no variant 1 to restore"
)]
#[case::moved_on(
    Editor::Owner,
    "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    "remove = [0]",
    StatusCode::CONFLICT,
    "runs"
)]
#[case::stranger(
    Editor::Stranger,
    "?base_plan_id={head}",
    MLX_700M_VARIANT,
    StatusCode::FORBIDDEN,
    ""
)]
#[tokio::test]
async fn a_refused_edit_files_nothing(
    #[case] who: Editor,
    #[case] query: &str,
    #[case] edit: &str,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let signer = match who {
        Editor::Owner => owner,
        Editor::Stranger => editor(&state).await?,
    };
    let query = query.replace("{head}", first.as_str());

    let resp = edit_variants(&state, &signer, &campaign_id, &query, edit).await?;
    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");

    assert_eq!(head(&state, &campaign_id).await?, Some(first.clone()));
    assert!(!state.plan_store.has_cancel_marker(&first).await?);
    assert_eq!(
        pipette_store::drain_plans(state.plan_store.as_ref(), None)
            .await?
            .items
            .len(),
        1,
        "a refused edit filed a plan"
    );
    Ok(())
}

/// A revision under a `Content-Type` its route does not read is a `415` that
/// files nothing: `…/plans`, `…/variants` and `PUT …/document` each read a plan
/// document alone. JSON job bodies are a `415` too, so they leave the campaign
/// on its plan. `{head}` in a suffix stands for the campaign's plan.
#[rstest]
#[case::plans_as_json(
    "POST",
    "/plans",
    Some("application/json"),
    r#"{"jobs": []}"#,
    DOCUMENT_TYPE
)]
#[case::plans_as_plain_text("POST", "/plans", Some("text/plain"), ONE_CELL_PLAN, DOCUMENT_TYPE)]
#[case::plans_untyped("POST", "/plans", None, ONE_CELL_PLAN, DOCUMENT_TYPE)]
#[case::variants_as_json(
    "POST",
    "/variants?base_plan_id={head}",
    Some("application/json"),
    MLX_700M_VARIANT,
    DOCUMENT_TYPE
)]
#[case::variants_as_plain_text(
    "POST",
    "/variants?base_plan_id={head}",
    Some("text/plain"),
    MLX_700M_VARIANT,
    DOCUMENT_TYPE
)]
#[case::variants_untyped(
    "POST",
    "/variants?base_plan_id={head}",
    None,
    MLX_700M_VARIANT,
    DOCUMENT_TYPE
)]
#[case::document_as_json(
    "PUT",
    "/document?base_plan_id={head}",
    Some("application/json"),
    ONE_CELL_PLAN,
    DOCUMENT_TYPE
)]
#[case::document_untyped(
    "PUT",
    "/document?base_plan_id={head}",
    None,
    ONE_CELL_PLAN,
    DOCUMENT_TYPE
)]
#[tokio::test]
async fn a_revision_of_a_type_its_route_does_not_read_files_nothing(
    #[case] method: &str,
    #[case] suffix: &str,
    #[case] content_type: Option<&str>,
    #[case] body: &str,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let uri = format!(
        "/api/v1/campaigns/{campaign_id}{}",
        suffix.replace("{head}", first.as_str())
    );

    let (key, user_id, key_id) = &owner;
    let headers = user_auth_headers(key, user_id, key_id, method, &uri)?;
    let resp = raw_request(
        &state,
        method,
        &uri,
        &headers,
        content_type,
        body.to_string(),
    )
    .await?;
    let status = resp.status();
    let answered = body_json(resp).await?;
    assert_eq!(
        (status, &answered["error"]),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, &json!(error)),
        "{method} {uri} as {content_type:?}"
    );
    assert_still_on(&state, &campaign_id, &first).await
}

/// What `POST /api/v1/campaigns/{campaign_id}/plans`, `…/variants` and
/// `PUT …/document` tell a body they do not read.
const DOCUMENT_TYPE: &str = "send the plan as an application/toml plan document";

/// A campaign whose plan was filed as job bodies has no document to edit, so
/// an edit is a `409` rather than a guess at what the bodies came from.
#[tokio::test]
async fn an_edit_of_a_plan_filed_as_job_bodies_is_a_conflict() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = editor(&state).await?;
    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let bodies = file_job_bodies(&state, &owner, &campaign_id, &client_id, "bodies").await?;

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={bodies}"),
        "remove = [0]",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains("no stored plan document"), "{message}");
    assert_eq!(head(&state, &campaign_id).await?, Some(bodies));
    Ok(())
}

/// Create a campaign as `signer`, and rewrite its first plan's stored manifest
/// with `edit`, as the CLI or an older build would have left it.
async fn created_with_head(
    state: &AppState,
    signer: &UserSigner,
    edit: impl FnOnce(&mut pipette_core::plan::PlanManifest),
) -> anyhow::Result<(String, PlanId)> {
    let created = created_campaign(state, signer, &[("name", "Nightly")]).await?;
    let mut head = manifest(state, &created.1).await?;
    edit(&mut head);
    state.plan_store.put_plan(&head).await?;
    Ok(created)
}

/// Whose campaign a variant edit revises in [`an_edit_takes_the_head_plans_grants`],
/// and who filed the plan it runs.
#[derive(Clone, Copy, Debug)]
enum Whose {
    /// The editor's own campaign, running a plan the editor filed.
    Own,
    /// The editor's own campaign, running a plan nobody filed: the CLI records
    /// no submitter.
    Nobody,
    /// Another account's campaign, which only a server admin administers.
    Another,
    /// The organization's campaign, running a plan a colleague filed. The
    /// editor administers it.
    Colleague,
    /// The editor's own campaign, running a plan that names no campaign, so
    /// the campaign did not file it.
    Foreign,
}

/// A variant edit reads and cancels the plan it replaces, so it also takes the
/// plan grants for a head the campaign filed, whoever filed it (the editor
/// too), the `_any_` grants for any other, and the full tier to read the
/// document. A dry run is refused the same.
#[rstest]
#[case::own_without_cancel(
    Whose::Own,
    &[UserPermission::SubmitPlans, UserPermission::ReadClients],
    "",
    StatusCode::FORBIDDEN,
    "cancel_plans"
)]
#[case::own_with_the_plan_grants(
    Whose::Own,
    &[UserPermission::SubmitPlans, UserPermission::CancelPlans, UserPermission::ReadClients],
    "",
    StatusCode::CREATED,
    ""
)]
#[case::own_withheld(
    Whose::Own,
    &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    "",
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::own_dry_run_withheld(
    Whose::Own,
    &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    "&dry_run=true",
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::nobody_with_the_plan_grants(
    Whose::Nobody,
    &[UserPermission::SubmitPlans, UserPermission::CancelPlans, UserPermission::ReadClients],
    "",
    StatusCode::CREATED,
    ""
)]
#[case::colleague_without_cancel(
    Whose::Colleague,
    &[UserPermission::ManageOrgs, UserPermission::SubmitPlans, UserPermission::ReadClients],
    "",
    StatusCode::FORBIDDEN,
    "cancel_plans"
)]
#[case::colleague_with_the_plan_grants(
    Whose::Colleague,
    &[
        UserPermission::ManageOrgs,
        UserPermission::SubmitPlans,
        UserPermission::CancelPlans,
        UserPermission::ReadClients,
    ],
    "",
    StatusCode::CREATED,
    ""
)]
#[case::colleague_dry_run_with_the_plan_grants(
    Whose::Colleague,
    &[
        UserPermission::ManageOrgs,
        UserPermission::SubmitPlans,
        UserPermission::CancelPlans,
        UserPermission::ReadClients,
    ],
    "&dry_run=true",
    StatusCode::OK,
    ""
)]
#[case::colleague_withheld(
    Whose::Colleague,
    &[UserPermission::ManageOrgs, UserPermission::SubmitPlans, UserPermission::CancelPlans],
    "",
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::colleague_dry_run_withheld(
    Whose::Colleague,
    &[UserPermission::ManageOrgs, UserPermission::SubmitPlans, UserPermission::CancelPlans],
    "&dry_run=true",
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::another_with_the_plan_grants(
    Whose::Another,
    &[
        UserPermission::SubmitPlans,
        UserPermission::ManageAnyOrgs,
        UserPermission::CancelPlans,
        UserPermission::ReadClients,
    ],
    "",
    StatusCode::FORBIDDEN,
    "read_any_plans"
)]
#[case::another_without_cancel(
    Whose::Another,
    &[
        UserPermission::SubmitPlans,
        UserPermission::ManageAnyOrgs,
        UserPermission::ReadAnyPlans,
        UserPermission::CancelPlans,
        UserPermission::ReadClients,
    ],
    "",
    StatusCode::FORBIDDEN,
    "cancel_any_plans"
)]
#[case::another_dry_run_without_cancel(
    Whose::Another,
    &[
        UserPermission::SubmitPlans,
        UserPermission::ManageAnyOrgs,
        UserPermission::ReadAnyPlans,
        UserPermission::CancelPlans,
        UserPermission::ReadClients,
    ],
    "&dry_run=true",
    StatusCode::FORBIDDEN,
    "cancel_any_plans"
)]
#[case::another_with_the_any_grants(
    Whose::Another,
    &[
        UserPermission::SubmitPlans,
        UserPermission::ManageAnyOrgs,
        UserPermission::ReadAnyPlans,
        UserPermission::CancelAnyPlans,
        UserPermission::ReadClients,
    ],
    "",
    StatusCode::CREATED,
    ""
)]
#[case::foreign_with_the_plan_grants(
    Whose::Foreign,
    &[UserPermission::SubmitPlans, UserPermission::CancelPlans, UserPermission::ReadClients],
    "",
    StatusCode::FORBIDDEN,
    "read_any_plans"
)]
#[case::foreign_without_cancel_any(
    Whose::Foreign,
    &[
        UserPermission::SubmitPlans,
        UserPermission::ReadAnyPlans,
        UserPermission::CancelPlans,
        UserPermission::ReadClients,
    ],
    "",
    StatusCode::FORBIDDEN,
    "cancel_any_plans"
)]
#[case::foreign_with_the_any_grants(
    Whose::Foreign,
    &[
        UserPermission::SubmitPlans,
        UserPermission::ReadAnyPlans,
        UserPermission::CancelAnyPlans,
        UserPermission::ReadClients,
    ],
    "",
    StatusCode::CREATED,
    ""
)]
#[tokio::test]
async fn an_edit_takes_the_head_plans_grants(
    #[case] whose: Whose,
    #[case] grants: &[UserPermission],
    #[case] query: &str,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let signer = registered_user(&state, grants).await?;
    let (campaign_id, first) = match whose {
        Whose::Own => created_campaign(&state, &signer, &[("name", "Nightly")]).await?,
        Whose::Nobody => {
            created_with_head(&state, &signer, |head| head.submitted_by = None).await?
        }
        Whose::Foreign => {
            created_with_head(&state, &signer, |head| head.campaign_id = None).await?
        }
        Whose::Another => {
            created_campaign(&state, &editor(&state).await?, &[("name", "Nightly")]).await?
        }
        Whose::Colleague => {
            let admin = server_admin(&state).await?;
            let acme = create_org(&state, &admin, "acme").await?;
            set_user_org(&state, &signer.1, Some(acme.parse()?)).await?;
            let colleague = editor(&state).await?;
            set_user_org(&state, &colleague.1, Some(acme.parse()?)).await?;
            created_campaign(&state, &colleague, &[("name", "Team"), ("owner", &acme)]).await?
        }
    };

    let resp = edit_variants(
        &state,
        &signer,
        &campaign_id,
        &format!("?base_plan_id={first}{query}"),
        MLX_700M_VARIANT,
    )
    .await?;

    assert_eq!(resp.status(), status);
    if status == StatusCode::FORBIDDEN {
        let message = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(message.contains(error), "{message}");
        assert_eq!(head(&state, &campaign_id).await?, Some(first.clone()));
        assert!(!state.plan_store.has_cancel_marker(&first).await?);
        assert_eq!(
            pipette_store::drain_plans(state.plan_store.as_ref(), None)
                .await?
                .items
                .len(),
            1,
            "a refused edit filed a plan"
        );
    }
    Ok(())
}

/// The team's administrator holding every grant a revision takes but
/// `submit_plans` revises nothing: a variant edit and a re-run, dry run or not.
#[rstest]
#[case::variant_edit("/variants?base_plan_id={head}", "application/toml", MLX_700M_VARIANT)]
#[case::variant_edit_dry_run(
    "/variants?base_plan_id={head}&dry_run=true",
    "application/toml",
    MLX_700M_VARIANT
)]
#[case::rerun("/rerun", "application/json", r#"{"job_ids": ["{job}"]}"#)]
#[case::rerun_dry_run("/rerun?dry_run=true", "application/json", r#"{"job_ids": ["{job}"]}"#)]
#[tokio::test]
async fn an_org_admin_without_submit_plans_revises_nothing(
    #[case] suffix: &str,
    #[case] content_type: &str,
    #[case] body: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let team_admin = registered_user(
        &state,
        &[
            UserPermission::ManageOrgs,
            UserPermission::ReadPlans,
            UserPermission::CancelPlans,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    set_user_org(&state, &team_admin.1, Some(acme.parse()?)).await?;
    let colleague = editor(&state).await?;
    set_user_org(&state, &colleague.1, Some(acme.parse()?)).await?;
    let (campaign_id, first) =
        created_campaign(&state, &colleague, &[("name", "Team"), ("owner", &acme)]).await?;
    let job = manifest(&state, &first).await?.job_ids[0].clone();
    let uri = format!(
        "/api/v1/campaigns/{campaign_id}{}",
        suffix.replace("{head}", first.as_str())
    );

    let (key, user_id, key_id) = &team_admin;
    let headers = user_auth_headers(key, user_id, key_id, "POST", &uri)?;
    let body = body.replace("{job}", job.as_str());
    let resp = raw_request(&state, "POST", &uri, &headers, Some(content_type), body).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user lacks the submit_plans permission"
    );
    assert_still_on(&state, &campaign_id, &first).await?;

    // It administers the campaign all the same: the refusal is the plan grant.
    assert_eq!(
        rename(&state, &team_admin, &campaign_id, "Team v2")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

/// A campaign an edit cannot address: none by that id is a `404`, and one that
/// runs no plan is a `409`, as the record says before any plan is read.
#[rstest]
#[case::unknown(false, StatusCode::NOT_FOUND, "no campaign")]
#[case::runs_no_plan(true, StatusCode::CONFLICT, "runs no plan")]
#[tokio::test]
async fn an_edit_of_a_campaign_with_no_plan_to_revise_is_refused(
    #[case] exists: bool,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let campaign_id = if exists {
        put_idle_campaign(&state, &owner.1, "Not started").await?
    } else {
        CampaignId::mint()
    };

    let resp = edit_variants(
        &state,
        &owner,
        campaign_id.as_str(),
        "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        MLX_700M_VARIANT,
    )
    .await?;

    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_nothing_staged(&state).await?;
    Ok(())
}

// ── document revisions ──────────────────────────────────────────────────────

async fn revise_document(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    query: &str,
    document: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    put_plan_document(
        state,
        signer,
        &format!("/api/v1/campaigns/{campaign_id}/document{query}"),
        document,
    )
    .await
}

/// A campaign of `signer`'s running [`ONE_CELL_PLAN`] routed to `tag`, so its
/// job is its own, and that job finished: the campaign's plan and the job.
async fn finished_donor(
    state: &AppState,
    signer: &UserSigner,
    tag: &str,
) -> anyhow::Result<(PlanId, JobId)> {
    let routed = ONE_CELL_PLAN.replace(
        "requires = [\"os:macos\"]",
        &format!("requires = [\"os:macos\", \"tag:{tag}\"]"),
    );
    let (_, plan) = created_campaign_from(state, signer, &[("name", tag)], &routed).await?;
    let job = manifest(state, &plan)
        .await?
        .job_ids
        .first()
        .cloned()
        .context("the campaign's job")?;
    pipette_store_testkit::finish_job_with_success_on_build(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &job,
        "prefill_throughput",
        None,
    )
    .await?;
    Ok((plan, job))
}

/// `inherit_from` lets a revision take another campaign's finished run of a
/// cell in place of its own queued job. A plan the caller cannot read is
/// refused as one that is not there, and files nothing.
#[tokio::test]
async fn a_revision_inherits_another_campaigns_finished_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let queued = manifest(&state, &first).await?.job_ids[0].clone();
    let (donor, ran) = finished_donor(&state, &owner, "lab-east").await?;
    let (theirs, _) = finished_donor(&state, &editor(&state).await?, "lab-sf").await?;

    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}&inherit_from={theirs}"),
        ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(head(&state, &campaign_id).await?, Some(first.clone()));

    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}&inherit_from={donor}&dry_run=true"),
        ONE_CELL_PLAN,
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inherited_job_count"], 1);
    assert_eq!(body["restored_job_count"], 0);
    assert_eq!(body["new_job_count"], 0);
    assert_eq!(body["dropped_job_ids"], json!([queued.as_str()]));
    assert_eq!(
        body["held_jobs"],
        json!([{"body_index": 0, "plan_id": donor.as_str(), "job_id": ran.as_str()}])
    );

    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}&inherit_from={donor}"),
        ONE_CELL_PLAN,
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    assert_eq!(manifest(&state, &revision).await?.job_ids, vec![ran]);
    assert_eq!(head(&state, &campaign_id).await?, Some(revision));
    Ok(())
}

/// A revision can inherit from at most 32 plans, including plans in
/// `add_from` and `restore`. The server refuses an edit above the limit
/// before it reads a plan.
#[tokio::test]
async fn a_revision_inherits_from_at_most_32_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let named = |count: usize| {
        (0..count)
            .map(|n| format!("plan-missing-{n}"))
            .collect::<Vec<_>>()
            .join(",")
    };

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!(
            "?base_plan_id={first}&inherit_from={}&add_from=plan-missing-32",
            named(32)
        ),
        "",
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("at most 32"), "{body}");

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!(
            "?base_plan_id={first}&inherit_from={}&add_from=plan-missing-31&restore=plan-missing-32:0",
            named(31)
        ),
        "",
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("at most 32"), "{body}");

    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}&inherit_from={}", named(32)),
        ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(head(&state, &campaign_id).await?, Some(first));
    Ok(())
}

/// A variant edit inherits as a document revision does: the cell a named
/// plan ran takes its run, and the added cell queues.
#[tokio::test]
async fn a_variant_edit_inherits_another_campaigns_finished_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let queued = manifest(&state, &first).await?.job_ids;
    let (donor, ran) = finished_donor(&state, &owner, "lab-east").await?;
    let query = format!("?base_plan_id={first}&inherit_from=%20{donor}%20,");

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("{query}&dry_run=true"),
        MLX_700M_VARIANT,
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inherited_job_count"], 1);
    assert_eq!(body["new_job_count"], 1);
    assert_eq!(body["dropped_job_ids"], json!(queued));

    let resp = edit_variants(&state, &owner, &campaign_id, &query, MLX_700M_VARIANT).await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["dropped_job_count"], 1);
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    let held = manifest(&state, &revision).await?.job_ids;
    assert_eq!(held.len(), 2);
    assert!(held.contains(&ran), "the donor's run is held");
    assert!(
        queued.iter().all(|job_id| !held.contains(job_id)),
        "the queued job stopped"
    );
    Ok(())
}

/// Create a plan with one variant and finish the run it can offer to an edit.
async fn finished_variant_donor(
    state: &AppState,
    owner: &UserSigner,
) -> anyhow::Result<(PlanId, Vec<JobId>)> {
    let other = format!("benchmarks = [\"prefill_throughput_256\"]\n{MLX_700M_VARIANT}");
    let (_, donor) = created_campaign_from(state, owner, &[("name", "Other")], &other).await?;
    let ran = manifest(state, &donor).await?.job_ids;
    futures::future::try_join_all(ran.iter().map(|job_id| {
        pipette_store_testkit::finish_job_with_success_on_build(
            state.submission_store.as_ref(),
            state.todo_store.as_ref(),
            job_id,
            "prefill_throughput",
            None,
        )
    }))
    .await?;
    Ok((donor, ran))
}

/// `add_from` appends another campaign's variants, which take its finished
/// runs, so an empty edit body extends the campaign and queues nothing. The
/// document route has no variants to append to, and refuses it.
#[tokio::test]
async fn a_variant_edit_adds_another_plans_variants() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let kept = manifest(&state, &first).await?.job_ids;
    let (donor, ran) = finished_variant_donor(&state, &owner).await?;
    let query = format!("?base_plan_id={first}&add_from={donor}");

    // The blocks the app appends: the variant holds the plan's list.
    let variants = format!("/api/v1/plans/{donor}/variants");
    let resp = pipette_web_testkit::signed_empty(&state, &owner, "GET", &variants).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("vary")
            .and_then(|value| value.to_str().ok()),
        Some("accept")
    );
    let blocks = String::from_utf8(
        http_body_util::BodyExt::collect(resp.into_body())
            .await?
            .to_bytes()
            .to_vec(),
    )?;
    assert!(blocks.starts_with("[[variants]]"), "{blocks}");
    assert!(
        blocks.contains(r#"benchmarks = ["prefill_throughput_256"]"#),
        "{blocks}"
    );
    assert!(blocks.contains("LFM2-700M-MLX-4bit"), "{blocks}");
    let stranger = editor(&state).await?;
    let resp = pipette_web_testkit::signed_empty(&state, &stranger, "GET", &variants).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = revise_document(&state, &owner, &campaign_id, &query, ONE_CELL_PLAN).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("{query}&dry_run=true"),
        "",
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inherited_job_count"], 1);
    assert_eq!(body["new_job_count"], 0);
    assert!(
        body["document"]
            .as_str()
            .is_some_and(|document| document.contains("LFM2-700M-MLX-4bit")),
        "{body}"
    );

    let resp = edit_variants(&state, &owner, &campaign_id, &query, "").await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    let held: std::collections::BTreeSet<JobId> = manifest(&state, &revision)
        .await?
        .job_ids
        .into_iter()
        .collect();
    assert_eq!(held, kept.into_iter().chain(ran).collect());
    Ok(())
}

/// `restore` appends one variant of another plan, numbered as the JSON form of
/// `GET /api/v1/plans/{plan_id}/variants` lists them. The variant takes that
/// plan's finished run, so nothing queues, and the filed change names it. A
/// plan the caller cannot read causes the server to return 404.
#[tokio::test]
async fn a_variant_edit_restores_one_variant_of_another_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let kept = manifest(&state, &first).await?.job_ids;
    let (donor, ran) = finished_variant_donor(&state, &owner).await?;
    let (theirs, _) = finished_donor(&state, &editor(&state).await?, "lab-sf").await?;

    let variants = format!("/api/v1/plans/{donor}/variants");
    let (key, user_id, key_id) = &owner;
    let mut headers = user_auth_headers(key, user_id, key_id, "GET", &variants)?;
    headers.push(("Accept", "application/json".to_owned()));
    let resp = request_with_headers(&state, "GET", &variants, &headers).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("vary")
            .and_then(|value| value.to_str().ok()),
        Some("accept")
    );
    let listed = body_json(resp).await?;
    crate::api_schema::assert_conforms("PlanVariants", &listed)?;
    let blocks = listed["variants"]
        .as_array()
        .context("the variants array")?;
    assert_eq!(blocks.len(), 1, "{listed}");
    assert!(
        blocks[0]
            .as_str()
            .is_some_and(|block| block.starts_with("[[variants]]")),
        "{listed}"
    );

    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}&restore={theirs}:0"),
        "",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let query = format!("?base_plan_id={first}&restore={donor}:0");
    let resp = edit_variants(
        &state,
        &owner,
        &campaign_id,
        &format!("{query}&dry_run=true"),
        "",
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    crate::api_schema::assert_conforms("RevisionDryRun", &body)?;
    assert_eq!(body["inherited_job_count"], 1);
    assert_eq!(body["new_job_count"], 0);

    let resp = edit_variants(&state, &owner, &campaign_id, &query, "").await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    crate::api_schema::assert_conforms("CampaignRevised", &body)?;
    assert_eq!(
        body["plan"]["change"]["restored"],
        json!([{"plan_id": donor.as_str(), "variant_index": 0}])
    );
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    let held: std::collections::BTreeSet<JobId> = manifest(&state, &revision)
        .await?
        .job_ids
        .into_iter()
        .collect();
    assert_eq!(held, kept.into_iter().chain(ran).collect());
    Ok(())
}

/// A document revision changes what a variant edit cannot. Adding a benchmark
/// to the plan's list carries the cell it kept and adds one, files the
/// document as written, moves the campaign and cancels the plan it replaced.
/// Dropping the first benchmark then drops its job and carries the other.
#[tokio::test]
async fn a_document_revision_carries_what_it_still_describes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let first_job = manifest(&state, &first).await?.job_ids;

    let widened = format!(
        "# memory too\n{}",
        ONE_CELL_PLAN.replace(
            "[\"prefill_throughput_256\"]",
            "[\"prefill_throughput_256\", \"max_memory_usage_256\"]"
        )
    );
    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?note=rev-1&base_plan_id={first}"),
        &widened,
    )
    .await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["previous_plan_id"], json!(first.as_str()));
    assert_eq!(body["carried_job_count"], 1);
    assert_eq!(body["dropped_job_count"], 0);
    assert_eq!(body["cancel_requested"], true);
    assert_eq!(body["plan"]["job_count"], 2);
    assert_eq!(body["plan"]["note"], "rev-1");
    assert_eq!(body["plan"]["change"], json!({"type": "document"}));
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;

    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    let revised = manifest(&state, &revision).await?;
    assert_eq!(revised.previous_plan_id, Some(first.clone()));
    assert!(
        revised.job_ids.contains(&first_job[0]),
        "the kept job carried"
    );
    let source = state
        .plan_store
        .get_plan_source(&revision)
        .await?
        .context("the revision's document")?;
    assert_eq!(source.text(), widened);
    assert!(state.plan_store.has_cancel_marker(&first).await?);

    // A retry whose first answer went missing finds its revision filed.
    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?note=rev-1&base_plan_id={first}"),
        &widened,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["changed"], json!(false));
    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    // Another document off the same base is a stale edit, not a retry.
    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}"),
        &format!("# another\n{widened}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let narrowed = widened.replace("\"prefill_throughput_256\", ", "");
    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={revision}"),
        &narrowed,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["carried_job_count"], 1);
    assert_eq!(body["dropped_job_count"], 1);
    let latest = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;
    assert!(
        !manifest(&state, &latest)
            .await?
            .job_ids
            .contains(&first_job[0]),
        "the dropped job is not held"
    );
    Ok(())
}

/// The document the campaign already runs is a success that files nothing:
/// `200` with the campaign and `changed: false`, the plan left running.
#[tokio::test]
async fn a_document_revision_that_changes_nothing_is_a_successful_no_op() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;

    let resp = revise_document(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={first}"),
        ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], json!(false));
    assert_eq!(body["current_plan_id"], json!(first.as_str()));

    assert_eq!(head(&state, &campaign_id).await?, Some(first.clone()));
    assert!(!state.plan_store.has_cancel_marker(&first).await?);
    assert_eq!(
        pipette_store::drain_plans(state.plan_store.as_ref(), None)
            .await?
            .items
            .len(),
        1,
        "an unchanged document filed a plan"
    );
    Ok(())
}

/// A revision refused before anything is filed: by the query string, by the
/// planner, previewed or not, by a campaign that moved off the plan the caller
/// read, or by the caller. Each case appends `variant` to [`ONE_CELL_PLAN`];
/// `{head}` in a query stands for the campaign's plan.
#[rstest]
#[case::no_base(
    Editor::Owner,
    "",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "base_plan_id"
)]
#[case::misspelt_dry_run(
    Editor::Owner,
    "?base_plan_id={head}&dry-run=true",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "dry-run"
)]
#[case::add_from(
    Editor::Owner,
    "?base_plan_id={head}&add_from={head}",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "add_from appends variants, so it is a parameter of a variant edit"
)]
#[case::empty_add_from(
    Editor::Owner,
    "?base_plan_id={head}&add_from=",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "add_from appends variants, so it is a parameter of a variant edit"
)]
#[case::restore(
    Editor::Owner,
    "?base_plan_id={head}&restore={head}:0",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "restore appends variants, so it is a parameter of a variant edit"
)]
// The query parses the list before the route refuses it, so the plan id is named first.
#[case::add_from_no_plan_id(
    Editor::Owner,
    "?base_plan_id={head}&add_from=bad!id",
    MLX_700M_VARIANT,
    StatusCode::BAD_REQUEST,
    "plan_id contains invalid character '!'"
)]
#[case::planner(
    Editor::Owner,
    "?base_plan_id={head}",
    VARIANT_WITHOUT_A_RUNTIME,
    StatusCode::BAD_REQUEST,
    "missing field `runtimes`"
)]
#[case::planner_dry_run(
    Editor::Owner,
    "?base_plan_id={head}&dry_run=true",
    VARIANT_WITHOUT_A_RUNTIME,
    StatusCode::BAD_REQUEST,
    "missing field `runtimes`"
)]
#[case::moved_on(
    Editor::Owner,
    "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    MLX_700M_VARIANT,
    StatusCode::CONFLICT,
    "runs"
)]
#[case::stranger(
    Editor::Stranger,
    "?base_plan_id={head}",
    MLX_700M_VARIANT,
    StatusCode::FORBIDDEN,
    ""
)]
#[tokio::test]
async fn a_refused_document_revision_files_nothing(
    #[case] who: Editor,
    #[case] query: &str,
    #[case] variant: &str,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let signer = match who {
        Editor::Owner => owner,
        Editor::Stranger => editor(&state).await?,
    };
    let query = query.replace("{head}", first.as_str());

    let document = format!("{ONE_CELL_PLAN}{variant}");
    let resp = revise_document(&state, &signer, &campaign_id, &query, &document).await?;
    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_still_on(&state, &campaign_id, &first).await
}

// ── re-runs ─────────────────────────────────────────────────────────────────

/// Post `body` to `POST /api/v1/campaigns/{campaign_id}/rerun` as `signer`.
async fn rerun(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    rerun_with(state, signer, campaign_id, "", body).await
}

/// [`rerun`], with `query` (`?…` or empty) on the URI.
async fn rerun_with(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    query: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        signer,
        "POST",
        &format!("/api/v1/campaigns/{campaign_id}/rerun{query}"),
        body,
    )
    .await
}

/// The campaign's head is `first` and nothing else was filed or cancelled:
/// what a refused re-run, or a dry run, leaves behind.
async fn assert_still_on(
    state: &AppState,
    campaign_id: &str,
    first: &PlanId,
) -> anyhow::Result<()> {
    assert_eq!(head(state, campaign_id).await?, Some(first.clone()));
    assert!(!state.plan_store.has_cancel_marker(first).await?);
    assert_eq!(
        pipette_store::drain_plans(state.plan_store.as_ref(), None)
            .await?
            .items
            .len(),
        1,
        "a plan was filed"
    );
    Ok(())
}

/// A dry run checks the jobs as a re-run does and answers with the document it
/// would file and the job bodies that expands to. It files nothing, and the
/// campaign stays on its plan.
#[tokio::test]
async fn a_dry_run_rerun_files_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &done,
    )
    .await?;

    let resp = rerun_with(
        &state,
        &owner,
        &campaign_id,
        "?dry_run=true",
        &json!({"job_ids": [done.as_str()]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "document")?.contains("prefill_throughput_256"),
        "{body}"
    );
    assert_eq!(body["bodies"].as_array().map(Vec::len), Some(1));
    assert_still_on(&state, &campaign_id, &first).await
}

/// A re-run of a finished job files the plan's document again as a revision
/// that runs the cell as a new queued job, moves the campaign onto it, and
/// cancels the plan it replaced.
#[tokio::test]
async fn a_rerun_runs_a_finished_job_again_and_cancels_what_it_replaced() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &done,
    )
    .await?;

    let resp = rerun(
        &state,
        &owner,
        &campaign_id,
        &json!({"job_ids": [done.as_str()], "note": "rerun-1"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["previous_plan_id"], json!(first.as_str()));
    assert_eq!(body["carried_job_count"], 0);
    assert_eq!(body["rerun_job_count"], 1);
    assert_eq!(body["dropped_job_count"], 0);
    assert_eq!(body["cancel_requested"], true);
    assert_eq!(body["plan"]["note"], "rerun-1");
    assert_eq!(
        body["plan"]["change"],
        json!({"type": "rerun", "job_count": 1})
    );
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;

    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    let revised = manifest(&state, &revision).await?;
    assert_eq!(revised.previous_plan_id, Some(first.clone()));
    assert_eq!(revised.job_ids.len(), 1);
    assert_ne!(revised.job_ids[0], done, "the cell runs as a new job");
    assert!(
        state
            .todo_store
            .get_avail_by_job(&revised.job_ids[0])
            .await?
            .is_some()
    );
    assert!(state.plan_store.has_cancel_marker(&first).await?);

    // The revision's jobs name the job each runs again.
    let resp = signed_empty(
        &state,
        &owner,
        "GET",
        &format!("/api/v1/plans/{revision}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let jobs = body_json(resp).await?;
    assert_eq!(
        jobs["jobs"][0]["job_id"],
        json!(revised.job_ids[0].as_str())
    );
    assert_eq!(
        jobs["jobs"][0]["plan"]["rerun_of"]["job_id"],
        json!(done.as_str())
    );
    Ok(())
}

/// `rerun_job_count` is 1, and the revision holds one new job for the cell
/// however often the id repeats.
#[tokio::test]
async fn a_job_named_twice_is_rerun_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &done,
    )
    .await?;

    let resp = rerun(
        &state,
        &owner,
        &campaign_id,
        &json!({"job_ids": [done.as_str(), done.as_str()]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["rerun_job_count"], 1, "{body}");
    assert_eq!(body["carried_job_count"], 0, "{body}");
    assert_eq!(body["dropped_job_count"], 0, "{body}");
    assert_eq!(body["plan"]["job_count"], 1, "{body}");
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;

    // The cell was minted afresh rather than carried over.
    let revised = manifest(&state, &revision).await?;
    assert_eq!(revised.job_ids.len(), 1);
    assert_ne!(revised.job_ids[0], done, "the cell runs as a new job");
    assert_eq!(state.todo_store.list_avail().await?.len(), 1);
    Ok(())
}

/// Who sends a re-run in [`a_refused_rerun_files_nothing`].
#[derive(Clone, Copy, Debug)]
enum Rerunner {
    Owner,
    /// Owns the campaign, and may not cancel the plan a re-run replaces.
    OwnerWithoutCancel,
    /// Owns the campaign at the withheld tier, so it may not read the document
    /// a re-run files again.
    OwnerWithheld,
    /// Files plans of its own, and does not administer this campaign.
    Stranger,
}

/// A re-run refused before anything is filed: by its body or query, by a job
/// the plan does not hold or that has not answered, or by the caller. A dry run
/// is refused where the re-run would be, and a misspelt `dry_run` must not file
/// for real. `{job}` in a body stands for the plan's one job, still queued.
#[rstest]
#[case::no_job(Rerunner::Owner, "", json!({"job_ids": []}), StatusCode::BAD_REQUEST, "names no job")]
#[case::unknown_field(
    Rerunner::Owner,
    "",
    json!({"job_ids": ["{job}"], "dry_run": true}),
    StatusCode::BAD_REQUEST,
    "dry_run"
)]
#[case::not_a_job_id(
    Rerunner::Owner,
    "",
    json!({"job_ids": ["a job"]}),
    StatusCode::BAD_REQUEST,
    "job_id"
)]
#[case::not_in_the_plan(
    Rerunner::Owner,
    "",
    json!({"job_ids": ["job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"]}),
    StatusCode::CONFLICT,
    "holds no job"
)]
#[case::still_queued(
    Rerunner::Owner,
    "",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "has no result yet"
)]
#[case::without_cancel(
    Rerunner::OwnerWithoutCancel,
    "",
    json!({"job_ids": ["{job}"]}),
    StatusCode::FORBIDDEN,
    "cancel_plans"
)]
#[case::withheld(
    Rerunner::OwnerWithheld,
    "",
    json!({"job_ids": ["{job}"]}),
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::stranger(Rerunner::Stranger, "", json!({"job_ids": ["{job}"]}), StatusCode::FORBIDDEN, "")]
#[case::still_queued_dry_run(
    Rerunner::Owner,
    "?dry_run=true",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "has no result yet"
)]
#[case::dry_run_not_a_bool(
    Rerunner::Owner,
    "?dry_run=maybe",
    json!({"job_ids": ["{job}"]}),
    StatusCode::BAD_REQUEST,
    "dry_run"
)]
#[case::misspelt_dry_run(
    Rerunner::Owner,
    "?dry-run=true",
    json!({"job_ids": ["{job}"]}),
    StatusCode::BAD_REQUEST,
    "dry-run"
)]
#[case::withheld_dry_run(
    Rerunner::OwnerWithheld,
    "?dry_run=true",
    json!({"job_ids": ["{job}"]}),
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::stranger_dry_run(
    Rerunner::Stranger,
    "?dry_run=true",
    json!({"job_ids": ["{job}"]}),
    StatusCode::FORBIDDEN,
    ""
)]
#[tokio::test]
async fn a_refused_rerun_files_nothing(
    #[case] who: Rerunner,
    #[case] query: &str,
    #[case] body: Value,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = match who {
        Rerunner::OwnerWithoutCancel => {
            registered_user(&state, &[UserPermission::SubmitPlans]).await?
        }
        Rerunner::OwnerWithheld => {
            registered_user(
                &state,
                &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
            )
            .await?
        }
        Rerunner::Owner | Rerunner::Stranger => editor(&state).await?,
    };
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let job = manifest(&state, &first).await?.job_ids[0].clone();
    let signer = match who {
        Rerunner::Stranger => editor(&state).await?,
        Rerunner::Owner | Rerunner::OwnerWithoutCancel | Rerunner::OwnerWithheld => owner,
    };
    let body: Value = serde_json::from_str(&body.to_string().replace("{job}", job.as_str()))?;

    let resp = rerun_with(&state, &signer, &campaign_id, query, &body).await?;
    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_still_on(&state, &campaign_id, &first).await
}

/// A re-run body that is not JSON, or not a re-run, is a `400` in the envelope
/// for the owner and never a `415` or `422`. A stranger is refused before the
/// body is read, so the same bytes tell it only `403`. `{job}` is the plan's job.
#[rstest]
#[case::malformed_json(
    Rerunner::Owner,
    Some("application/json"),
    r#"{"job_ids": ["#,
    StatusCode::BAD_REQUEST,
    "Failed to parse the request body as JSON"
)]
#[case::typed_as_toml(
    Rerunner::Owner,
    Some("application/toml"),
    r#"{"job_ids": ["{job}"]}"#,
    StatusCode::BAD_REQUEST,
    "Expected request with `Content-Type: application/json`"
)]
#[case::untyped(
    Rerunner::Owner,
    None,
    r#"{"job_ids": ["{job}"]}"#,
    StatusCode::BAD_REQUEST,
    "Expected request with `Content-Type: application/json`"
)]
#[case::no_job_ids(
    Rerunner::Owner,
    Some("application/json"),
    "{}",
    StatusCode::BAD_REQUEST,
    "name either job_ids or answered"
)]
#[case::stranger_malformed_json(
    Rerunner::Stranger,
    Some("application/json"),
    r#"{"job_ids": ["#,
    StatusCode::FORBIDDEN,
    "this campaign belongs to another owner"
)]
#[case::stranger_typed_as_toml(
    Rerunner::Stranger,
    Some("application/toml"),
    "job_ids = []",
    StatusCode::FORBIDDEN,
    "this campaign belongs to another owner"
)]
#[tokio::test]
async fn a_rerun_body_that_is_not_a_rerun_files_nothing(
    #[case] who: Rerunner,
    #[case] content_type: Option<&str>,
    #[case] body: &str,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let job = manifest(&state, &first).await?.job_ids[0].clone();
    let signer = match who {
        Rerunner::Stranger => editor(&state).await?,
        Rerunner::Owner | Rerunner::OwnerWithoutCancel | Rerunner::OwnerWithheld => owner,
    };
    let uri = format!("/api/v1/campaigns/{campaign_id}/rerun");

    let (key, user_id, key_id) = &signer;
    let headers = user_auth_headers(key, user_id, key_id, "POST", &uri)?;
    let body = body.replace("{job}", job.as_str());
    let resp = raw_request(&state, "POST", &uri, &headers, content_type, body).await?;
    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_still_on(&state, &campaign_id, &first).await
}

/// A re-run of a campaign with nothing it can re-run is a `409` that files
/// nothing: one that runs no plan, and one whose head was filed as job bodies
/// and so recorded no digest for its job, even a finished one.
#[tokio::test]
async fn a_rerun_with_nothing_to_rerun_from_is_a_conflict() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let owner = editor(&state).await?;

    let idle = put_idle_campaign(&state, &owner.1, "Not started").await?;
    let resp = rerun(
        &state,
        &owner,
        idle.as_str(),
        &json!({"job_ids": ["job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("campaign {idle} runs no plan to revise")
    );
    assert_nothing_staged(&state).await?;

    let campaign_id = create_campaign(&state, &owner, "Nightly", None).await?;
    let bodies = file_job_bodies(&state, &owner, &campaign_id, &client_id, "bodies").await?;
    let job = manifest(&state, &bodies).await?.job_ids[0].clone();
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &job,
    )
    .await?;
    let plans = pipette_store::drain_plans(state.plan_store.as_ref(), None)
        .await?
        .items
        .len();

    let resp = rerun(
        &state,
        &owner,
        &campaign_id,
        &json!({"job_ids": [job.as_str()]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("plan {bodies} holds no job {job} it can re-run or recalculate")
    );
    assert_eq!(head(&state, &campaign_id).await?, Some(bodies.clone()));
    assert!(!state.plan_store.has_cancel_marker(&bodies).await?);
    assert_eq!(
        pipette_store::drain_plans(state.plan_store.as_ref(), None)
            .await?
            .items
            .len(),
        plans,
        "a refused re-run filed a plan"
    );
    Ok(())
}

/// A document that no longer expands to the plan's cells is refused, dry run
/// or not, and files nothing.
#[rstest]
#[tokio::test]
async fn a_drifted_rerun_files_nothing(
    #[values("", "?dry_run=true")] query: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &done,
    )
    .await?;
    // The head records another digest for the cell, as if the catalog moved.
    let mut head = manifest(&state, &first).await?;
    head.content_keys.insert(done.clone(), "0".repeat(64));
    state.plan_store.put_plan(&head).await?;

    let body = json!({"job_ids": [done.as_str()]});
    let resp = rerun_with(&state, &owner, &campaign_id, query, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains("no longer expands"), "{message}");
    assert_still_on(&state, &campaign_id, &first).await
}

// ── reverts ─────────────────────────────────────────────────────────────────

/// Post `body` to `POST /api/v1/campaigns/{campaign_id}/revert{query}` as
/// `signer`.
async fn revert(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    query: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        signer,
        "POST",
        &format!("/api/v1/campaigns/{campaign_id}/revert{query}"),
        body,
    )
    .await
}

/// Create a campaign of `owner`'s with two cells, and run both cells to the end.
/// Then revise the document to drop the 700M cell. Return the campaign's id,
/// the first plan, the head, and the dropped job.
async fn dropped_a_finished_cell(
    state: &AppState,
    owner: &UserSigner,
) -> anyhow::Result<(String, PlanId, PlanId, JobId)> {
    let both = format!("{ONE_CELL_PLAN}{MLX_700M_VARIANT}");
    let (campaign_id, first) =
        created_campaign_from(state, owner, &[("name", "Nightly")], &both).await?;
    let jobs = manifest(state, &first).await?.job_ids;
    for job in &jobs {
        pipette_store_testkit::finish_job(
            state.submission_store.as_ref(),
            state.todo_store.as_ref(),
            job,
        )
        .await?;
    }
    let resp = revise_document(
        state,
        owner,
        &campaign_id,
        &format!("?base_plan_id={first}"),
        ONE_CELL_PLAN,
    )
    .await?;
    anyhow::ensure!(resp.status() == StatusCode::CREATED, "the revision failed");
    let body = body_json(resp).await?;
    let plan = body.get("plan").context("the revision reports its plan")?;
    let second = PlanId::try_new(json_str(plan, "plan_id")?)?;
    let kept = manifest(state, &second).await?.job_ids;
    let dropped = jobs
        .into_iter()
        .find(|job| !kept.contains(job))
        .context("the revision dropped a job")?;
    Ok((campaign_id, first, second, dropped))
}

/// A revert first runs a preview. It then files a copy of the first plan's
/// document. The dropped cell's finished job comes back with no new run. The
/// campaign moves onto the copy. The server cancels the head.
#[tokio::test]
async fn a_revert_brings_back_a_dropped_cells_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first, second, dropped) = dropped_a_finished_cell(&state, &owner).await?;
    let body = json!({"plan_id": first.as_str(), "note": "back to the first"});

    let resp = revert(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={second}&dry_run=true"),
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let preview = body_json(resp).await?;
    crate::api_schema::assert_conforms("RevisionDryRun", &preview)?;
    assert_eq!(preview["unchanged"], json!(false), "{preview}");
    assert_eq!(preview["carried_job_count"], 1, "{preview}");
    assert_eq!(preview["restored_job_count"], 1, "{preview}");
    assert_eq!(preview["new_job_count"], 0, "{preview}");
    assert_eq!(head(&state, &campaign_id).await?, Some(second.clone()));

    let resp = revert(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={second}"),
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let filed = body_json(resp).await?;
    crate::api_schema::assert_conforms("CampaignRevised", &filed)?;
    assert_eq!(filed["previous_plan_id"], json!(second.as_str()));
    assert_eq!(filed["cancel_requested"], true);
    assert_eq!(filed["plan"]["note"], "back to the first");
    assert_eq!(
        filed["plan"]["change"],
        json!({"type": "revert", "to_plan_id": first.as_str()})
    );
    let reverted = PlanId::try_new(json_str(&filed["plan"], "plan_id")?)?;

    assert_eq!(head(&state, &campaign_id).await?, Some(reverted.clone()));
    let jobs = manifest(&state, &reverted).await?.job_ids;
    assert_eq!(jobs.len(), 2);
    assert!(jobs.contains(&dropped), "the dropped cell's run came back");
    assert!(
        state.todo_store.list_avail().await?.is_empty(),
        "nothing runs again"
    );
    assert!(state.plan_store.has_cancel_marker(&second).await?);
    Ok(())
}

/// Which plan a refused revert names.
#[derive(Clone, Copy, Debug)]
enum RevertTo {
    First,
    Head,
    /// The plan of another campaign of the same owner.
    Foreign,
    /// A plan with no stored document, between the first plan and the head.
    NoDocument,
    /// A plan between the first plan and the head whose manifest is gone.
    Gone,
}

/// A revert that files nothing. The cases are:
///
/// - a revert to the head, which is a `200` no-op;
/// - a revert that its query or body refuses;
/// - a revert to a plan outside the campaign's history, or with no document;
/// - a revert with a `base_plan_id` that the campaign no longer runs;
/// - a revert that the caller may not file.
///
/// `{head}` and `{first}` in a query stand for the campaign's plans.
#[rstest]
#[case::the_head(
    Rerunner::Owner,
    "?base_plan_id={head}",
    RevertTo::Head,
    StatusCode::OK,
    r#""changed":false"#
)]
#[case::the_head_dry_run(
    Rerunner::Owner,
    "?base_plan_id={head}&dry_run=true",
    RevertTo::Head,
    StatusCode::OK,
    r#""unchanged":true"#
)]
#[case::another_campaigns_plan(
    Rerunner::Owner,
    "?base_plan_id={head}",
    RevertTo::Foreign,
    StatusCode::NOT_FOUND,
    "is not in the history of campaign"
)]
#[case::another_campaigns_plan_dry_run(
    Rerunner::Owner,
    "?base_plan_id={head}&dry_run=true",
    RevertTo::Foreign,
    StatusCode::NOT_FOUND,
    "is not in the history of campaign"
)]
#[case::a_plan_with_no_document(
    Rerunner::Owner,
    "?base_plan_id={head}",
    RevertTo::NoDocument,
    StatusCode::CONFLICT,
    "has no stored plan document"
)]
#[case::a_plan_whose_manifest_is_gone(
    Rerunner::Owner,
    "?base_plan_id={head}",
    RevertTo::Gone,
    StatusCode::CONFLICT,
    "has no stored plan document"
)]
#[case::moved_on(
    Rerunner::Owner,
    "?base_plan_id={first}",
    RevertTo::First,
    StatusCode::CONFLICT,
    "runs"
)]
#[case::no_base(
    Rerunner::Owner,
    "",
    RevertTo::First,
    StatusCode::BAD_REQUEST,
    "base_plan_id"
)]
#[case::without_cancel(
    Rerunner::OwnerWithoutCancel,
    "?base_plan_id={head}",
    RevertTo::First,
    StatusCode::FORBIDDEN,
    "cancel_plans"
)]
#[case::withheld(
    Rerunner::OwnerWithheld,
    "?base_plan_id={head}",
    RevertTo::First,
    StatusCode::FORBIDDEN,
    "read_clients"
)]
#[case::stranger(
    Rerunner::Stranger,
    "?base_plan_id={head}",
    RevertTo::First,
    StatusCode::FORBIDDEN,
    ""
)]
#[tokio::test]
async fn a_revert_that_files_nothing(
    #[case] who: Rerunner,
    #[case] query: &str,
    #[case] to: RevertTo,
    #[case] status: StatusCode,
    #[case] answer: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = match who {
        Rerunner::OwnerWithoutCancel => {
            registered_user(&state, &[UserPermission::SubmitPlans]).await?
        }
        Rerunner::OwnerWithheld => {
            registered_user(
                &state,
                &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
            )
            .await?
        }
        Rerunner::Owner | Rerunner::Stranger => editor(&state).await?,
    };
    // A document revision needs the grants that some cases withhold, so an
    // editor files the history and the campaign is then handed to `owner`.
    let author = editor(&state).await?;
    let (campaign_id, first, second, _) = dropped_a_finished_cell(&state, &author).await?;
    let mut campaign = state
        .campaign_store
        .get_campaign(&CampaignId::try_new(campaign_id.as_str())?)
        .await?
        .context("the campaign")?;
    campaign.owner = CampaignOwner::User {
        user_id: owner.1.clone(),
    };
    state.campaign_store.put_campaign(&campaign).await?;
    let (_, foreign) = created_campaign(&state, &author, &[("name", "Other")]).await?;
    let signer = match who {
        Rerunner::Stranger => editor(&state).await?,
        Rerunner::Owner | Rerunner::OwnerWithoutCancel | Rerunner::OwnerWithheld => owner,
    };
    let query = query
        .replace("{head}", second.as_str())
        .replace("{first}", first.as_str());
    let plan_id = match to {
        RevertTo::First => first.clone(),
        RevertTo::Head => second.clone(),
        RevertTo::Foreign => foreign,
        RevertTo::NoDocument | RevertTo::Gone => {
            let spliced = pipette_store_testkit::splice_without_document(
                state.plan_store.as_ref(),
                &second,
                1,
            )
            .await?
            .pop()
            .context("the spliced plan")?;
            if matches!(to, RevertTo::Gone) {
                pipette_store_testkit::local_fs_remove_plan(dir.path(), &spliced)?;
            }
            spliced
        }
    };
    let plans_before = pipette_store::drain_plans(state.plan_store.as_ref(), None)
        .await?
        .items
        .len();

    let resp = revert(
        &state,
        &signer,
        &campaign_id,
        &query,
        &json!({"plan_id": plan_id.as_str()}),
    )
    .await?;
    assert_eq!(resp.status(), status);
    let body = body_json(resp).await?.to_string();
    assert!(body.contains(answer), "{body}");

    assert_eq!(head(&state, &campaign_id).await?, Some(second.clone()));
    assert!(!state.plan_store.has_cancel_marker(&second).await?);
    assert_eq!(
        pipette_store::drain_plans(state.plan_store.as_ref(), None)
            .await?
            .items
            .len(),
        plans_before,
        "a plan was filed"
    );
    Ok(())
}

/// A revert body names its plan, and nothing else.
#[rstest]
#[case::no_plan_id(json!({}), "plan_id")]
#[case::not_a_plan_id(json!({"plan_id": "a plan"}), "plan_id")]
#[case::unknown_field(json!({"plan_id": "{first}", "carry_earlier": false}), "carry_earlier")]
#[tokio::test]
async fn a_revert_body_that_is_not_a_revert_is_a_bad_request(
    #[case] body: Value,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first, second, _) = dropped_a_finished_cell(&state, &owner).await?;
    let body: Value = serde_json::from_str(&body.to_string().replace("{first}", first.as_str()))?;

    let resp = revert(
        &state,
        &owner,
        &campaign_id,
        &format!("?base_plan_id={second}"),
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_eq!(head(&state, &campaign_id).await?, Some(second));
    Ok(())
}

// ── recalculations ──────────────────────────────────────────────────────────

/// Post `body` to `POST /api/v1/campaigns/{campaign_id}/recalculate{query}` as
/// `signer`; `query` is `?…` or empty.
async fn recalculate(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    query: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        signer,
        "POST",
        &format!("/api/v1/campaigns/{campaign_id}/recalculate{query}"),
        body,
    )
    .await
}

/// Record a success of `benchmark_type` for `job_id` and take it off the
/// queue, as a client that ran it would.
async fn answer(state: &AppState, job_id: &JobId, benchmark_type: &str) -> anyhow::Result<()> {
    pipette_store_testkit::finish_job_with_success(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        job_id,
        benchmark_type,
    )
    .await
}

/// The recalculated cell's new job is never queued; a copy of the named job's
/// record answers it.
#[tokio::test]
async fn a_recalculation_answers_a_new_job_from_the_record_and_cancels_what_it_replaced()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    answer(&state, &done, "eval").await?;

    let resp = recalculate(
        &state,
        &owner,
        &campaign_id,
        "",
        &json!({"job_ids": [done.as_str(), done.as_str()], "note": "rescored"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["previous_plan_id"], json!(first.as_str()));
    assert_eq!(body["carried_job_count"], 0);
    assert_eq!(body["rerun_job_count"], 0);
    assert_eq!(
        body["recalculated_job_count"], 1,
        "named twice, counted once"
    );
    assert_eq!(body["dropped_job_count"], 0);
    assert_eq!(body["cancel_requested"], true);
    assert_eq!(body["plan"]["note"], "rescored");
    assert_eq!(
        body["plan"]["change"],
        json!({"type": "recalculate", "job_count": 1})
    );
    let revision = PlanId::try_new(json_str(&body["plan"], "plan_id")?)?;

    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    let revised = manifest(&state, &revision).await?;
    assert_eq!(revised.previous_plan_id, Some(first.clone()));
    let [fresh] = revised.job_ids.as_slice() else {
        anyhow::bail!("not one job: {:?}", revised.job_ids);
    };
    assert_ne!(*fresh, done, "the cell is answered as a new job");
    assert!(
        state.todo_store.list_avail().await?.is_empty(),
        "nothing is queued"
    );
    let copy = state
        .submission_store
        .find_job(fresh)
        .await?
        .context("the new job's record")?;
    assert_eq!(copy.state, pipette_store::JobState::Incoming);
    assert_eq!(copy.body["job_id"], json!(fresh.as_str()));
    assert_eq!(copy.body["rescored_from"], json!(done.as_str()));
    assert_eq!(
        copy.body["completions"],
        json!([{"id": "s1", "completion": "@@@@"}])
    );
    assert!(state.plan_store.has_cancel_marker(&first).await?);

    // The revision lists the new job; what it was recalculated from is its
    // run's, which this server without `[duckdb]` does not read.
    let resp = signed_empty(
        &state,
        &owner,
        "GET",
        &format!("/api/v1/plans/{revision}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let jobs = body_json(resp).await?;
    assert_eq!(jobs["jobs"][0]["job_id"], json!(fresh.as_str()));
    assert_eq!(jobs["jobs"][0]["state"], json!("finished"));
    Ok(())
}

/// A dry run checks the jobs as a recalculation does and answers with the
/// document it would file. It files nothing and writes no record.
#[tokio::test]
async fn a_dry_run_recalculation_files_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let done = manifest(&state, &first).await?.job_ids[0].clone();
    answer(&state, &done, "eval").await?;

    let resp = recalculate(
        &state,
        &owner,
        &campaign_id,
        "?dry_run=true",
        &json!({"job_ids": [done.as_str()]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "document")?.contains("prefill_throughput_256"),
        "{body}"
    );
    assert_eq!(
        state
            .submission_store
            .list_incoming(NonZeroUsize::MIN)
            .await?,
        vec![]
    );
    assert_still_on(&state, &campaign_id, &first).await
}

/// What the plan's one job holds before [`a_refused_recalculation_files_nothing`]
/// posts.
#[derive(Clone, Copy, Debug)]
enum Recorded {
    Nothing,
    Failure,
    OtherBenchmark,
    Eval,
}

/// A recalculation refused before anything is filed: by its body, or by a job
/// whose record holds no eval completions to score again. A dry run is refused
/// where the recalculation would be. `{job}` stands for the plan's one job.
#[rstest]
#[case::no_job(Recorded::Eval, "", json!({"job_ids": []}), StatusCode::BAD_REQUEST, "names no job")]
#[case::unknown_field(
    Recorded::Eval,
    "",
    json!({"job_ids": ["{job}"], "jobs": "all"}),
    StatusCode::BAD_REQUEST,
    "jobs"
)]
#[case::not_in_the_plan(
    Recorded::Eval,
    "",
    json!({"job_ids": ["job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"]}),
    StatusCode::CONFLICT,
    "holds no job"
)]
#[case::still_queued(
    Recorded::Nothing,
    "",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "has no result yet"
)]
#[case::failed(
    Recorded::Failure,
    "",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "recorded no eval completions"
)]
#[case::not_an_eval(
    Recorded::OtherBenchmark,
    "",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "recorded no eval completions"
)]
#[case::not_an_eval_dry_run(
    Recorded::OtherBenchmark,
    "?dry_run=true",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "recorded no eval completions"
)]
#[tokio::test]
async fn a_refused_recalculation_files_nothing(
    #[case] recorded: Recorded,
    #[case] query: &str,
    #[case] body: Value,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = editor(&state).await?;
    let (campaign_id, first) = created_campaign(&state, &owner, &[("name", "Nightly")]).await?;
    let job = manifest(&state, &first).await?.job_ids[0].clone();
    match recorded {
        Recorded::Nothing => {}
        Recorded::Failure => {
            pipette_store_testkit::finish_job_with_failure(
                state.submission_store.as_ref(),
                state.todo_store.as_ref(),
                &job,
            )
            .await?;
        }
        Recorded::OtherBenchmark => answer(&state, &job, "prefill_throughput").await?,
        Recorded::Eval => answer(&state, &job, "eval").await?,
    }
    let body: Value = serde_json::from_str(&body.to_string().replace("{job}", job.as_str()))?;

    let resp = recalculate(&state, &owner, &campaign_id, query, &body).await?;
    assert_eq!(resp.status(), status);
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{message}");
    assert_eq!(
        state
            .submission_store
            .list_incoming(NonZeroUsize::MIN)
            .await?,
        vec![]
    );
    assert_still_on(&state, &campaign_id, &first).await
}
