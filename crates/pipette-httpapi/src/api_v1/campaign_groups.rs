//! `/api/v1/campaign-groups`, kept on a `[database]` alone: without one every
//! route answers `404`. A group takes the rights of its owner's campaigns:
//! reading it is reading them, renaming or deleting it is administering their
//! records, changing its members or its policies is filing into them, and
//! leaving a policy that re-runs or marks on, or adding a member while one is
//! on, is also re-running them. A write names the group, and its owner, which
//! puts it in that organization's log.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use futures::stream::{self, StreamExt, TryStreamExt};
use jiff::Timestamp;
use strum::IntoEnumIterator as _;

use pipette_core::campaign::{Campaign, CampaignGroup, CampaignOwner, GroupPolicyState};
use pipette_pipeline::campaign_admin::REPLACE_ATTEMPTS;
use pipette_pipeline::group_policies::{self, Evaluation, Hold, Pass};
use pipette_pipeline::model_params::ModelParamTable;
use pipette_pipeline::org_catalog::load_server_catalog;
use pipette_plan_types::{
    AddCampaignGroupMembersRequest, AuditTarget, CampaignGroupId, CampaignGroupPolicies,
    CampaignGroupPolicy, CampaignGroupPolicyPreview, CampaignGroupRerunResults, CampaignGroupView,
    CampaignId, CampaignView, Changed, CreateCampaignGroupRequest, ExpiresAt, ListPage,
    MemberPolicyPreview, OwnedListQuery, PolicyBase, PolicyFiling, PolicyKind, PolicyMemberState,
    RerunResultsQuery, SetCampaignGroupPolicyRequest, UpdateCampaignGroupRequest, UserPermission,
};
use pipette_store::{CampaignGroupStore, CampaignInAnotherGroupError, STORAGE_CONCURRENCY};

use crate::api_v1::campaigns::{
    check_name, owner_grants, require, require_owner_files_plans, view,
};
use crate::api_v1::plans::reads::JobWindow;
use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, user_org};
use crate::campaign_access::{CampaignAuthority, CampaignReach};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery, Parsed};
use crate::owner::{OwnedPage, page_within_reach, require_destination};
use crate::state::AppState;

const NEEDS_DATABASE: &str = "campaign groups are kept only on a [database]";
const NEEDS_DUCKDB: &str = "the policy preview is not served: no [duckdb] is configured";
const ANOTHER_OWNER: &str = "this campaign group belongs to another owner";

fn groups(state: &AppState) -> Result<&dyn CampaignGroupStore, AppError> {
    state
        .campaign_group_store
        .as_deref()
        .ok_or_else(|| AppError::NotFound(NEEDS_DATABASE.into()))
}

async fn require_group(
    store: &dyn CampaignGroupStore,
    group_id: &CampaignGroupId,
) -> Result<CampaignGroup, AppError> {
    store
        .get_group(group_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the campaign group")))?
        .ok_or_else(|| AppError::NotFound(format!("no campaign group {group_id}")))
}

/// The group a read names, if the caller reads its owner's campaigns, with the
/// caller's reach.
async fn for_read(
    state: &AppState,
    store: &dyn CampaignGroupStore,
    caller: Caller<'_>,
    group_id: &CampaignGroupId,
) -> Result<(CampaignReach, CampaignGroup), AppError> {
    let reach = CampaignReach::over_records(state, caller).await?;
    let group = require_group(store, group_id).await?;
    if reach.admits_owner(&group.owner) {
        return Ok((reach, group));
    }
    tracing::warn!(
        %group_id,
        owner = %group.owner.label(),
        "refused: the campaign group is outside the caller's reach"
    );
    // A `403`, not a `404`: a caller naming a minted id was given it.
    Err(AppError::Forbidden(
        match reach {
            CampaignReach::Listed { .. } => "a campaign reader reads no campaign group",
            CampaignReach::Any | CampaignReach::Own { .. } => ANOTHER_OWNER,
        }
        .into(),
    ))
}

/// The group a rename or delete names, if the caller administers its owner's campaigns.
async fn for_record_write(
    state: &AppState,
    store: &dyn CampaignGroupStore,
    caller: Caller<'_>,
    audit: &Audit,
    group_id: &CampaignGroupId,
) -> Result<CampaignGroup, AppError> {
    let authority = CampaignAuthority::resolve(state, caller).await?;
    let group = require_group(store, group_id).await?;
    audit
        .involve(state, [AuditTarget::from(group.owner.clone())])
        .await;
    authority
        .require_record_owner(
            state,
            caller.rights,
            &group.owner,
            group_id.as_str(),
            ANOTHER_OWNER,
        )
        .await?;
    Ok(group)
}

/// The group a member or policy change names, if the caller files into its owner's campaigns.
async fn for_member_write(
    state: &AppState,
    store: &dyn CampaignGroupStore,
    caller: Caller<'_>,
    audit: &Audit,
    group_id: &CampaignGroupId,
) -> Result<(CampaignAuthority, CampaignGroup), AppError> {
    let authority = CampaignAuthority::resolve(state, caller).await?;
    let group = require_group(store, group_id).await?;
    audit
        .involve(state, [AuditTarget::from(group.owner.clone())])
        .await;
    authority.require_run_owner(&group.owner, group_id.as_str(), ANOTHER_OWNER)?;
    Ok((authority, group))
}

/// The grants a re-run of `group`'s owner's campaigns takes: queue-maintenance
/// re-runs and marks the members with the host's authority while a policy that
/// does is on.
fn require_rerun_grants(
    caller: Caller<'_>,
    authority: &CampaignAuthority,
    group: &CampaignGroup,
) -> Result<(), AppError> {
    owner_grants(authority, &group.owner)
        .into_iter()
        .chain([UserPermission::ReadClients])
        .try_for_each(|grant| caller.rights.require(grant))
}

// POST /api/v1/campaign-groups
pub async fn create_campaign_group(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiJson(body): ApiJson<CreateCampaignGroupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let authority = CampaignAuthority::resolve(&state, active.caller()).await?;
    check_name(&body.name)?;
    let owner = body
        .owner
        .map_or_else(|| authority.own_owner(), CampaignOwner::from);
    require_destination(
        &state,
        &authority,
        &owner,
        "you may only create campaign groups for yourself, your organization, or a member of \
         the organization you administer",
    )
    .await?;
    require_owner_files_plans(&state, &owner).await?;
    audit
        .involve(&state, [AuditTarget::from(owner.clone())])
        .await;
    let group = CampaignGroup::new(body.name, owner, Timestamp::now());
    store
        .create_group(&group)
        .await
        .map_err(|e| AppError::internal(e.context("failed to write the campaign group")))?;
    let made = AuditTarget::CampaignGroup {
        group_id: group.group_id.clone(),
    };
    audit.involve(&state, [made]).await;
    tracing::info!(group_id = %group.group_id, "campaign group created");
    Ok((
        StatusCode::CREATED,
        Json(pipette_plan_types::CampaignGroup::from(group)),
    ))
}

// GET /api/v1/campaign-groups
pub async fn list_campaign_groups(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiQuery(query): ApiQuery<OwnedListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let caller = active.caller();
    let reach = CampaignReach::over_records(&state, caller).await?;
    let OwnedPage { page, .. } = page_within_reach(
        &state.config,
        query,
        &caller.user.user_id,
        |group: &CampaignGroup| reach.admits_owner(&group.owner),
        "campaign groups",
        |after, limit| async move { store.list_groups(after.as_ref(), limit).await },
    )
    .await?;
    Ok(Json(ListPage {
        items: page
            .items
            .into_iter()
            .map(pipette_plan_types::CampaignGroup::from)
            .collect(),
        next_cursor: page.next_cursor,
        unreadable: page.unreadable,
    }))
}

/// The group's member campaigns that `reach` admits, in member order.
async fn readable_members(
    state: &AppState,
    store: &dyn CampaignGroupStore,
    reach: &CampaignReach,
    group_id: &CampaignGroupId,
) -> Result<Vec<Campaign>, AppError> {
    let campaign_ids = store
        .members(group_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the group's members")))?;
    let campaigns = &state.campaign_store;
    // shortcut: a hand-off racing an add, or one whose group removal failed and was
    // not retried, can leave a member of another owner, and a hand-off racing
    // another can take out a member that has the group's owner. Reads filter
    // members by reach and `group_policies::evaluate` reads one whose owner
    // differs from the group's as `other_owner`, until the add's owner check and
    // insert, and the hand-off's owner write and group removal, each run in one
    // store transaction.
    stream::iter(campaign_ids)
        .map(|campaign_id| async move { campaigns.get_campaign(&campaign_id).await })
        .buffered(STORAGE_CONCURRENCY)
        .try_filter_map(
            |campaign| async move { Ok(campaign.filter(|campaign| reach.admits(campaign))) },
        )
        .try_collect()
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the group's campaigns")))
}

// GET /api/v1/campaign-groups/{group_id}
pub async fn get_campaign_group(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let (reach, group) = for_read(&state, store, active.caller(), &group_id).await?;
    let members: Vec<CampaignView> = readable_members(&state, store, &reach, &group_id)
        .await?
        .into_iter()
        .map(view)
        .collect();
    Ok(Json(CampaignGroupView {
        group: group.into(),
        members,
    }))
}

// PATCH /api/v1/campaign-groups/{group_id}
pub async fn update_campaign_group(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
    ApiJson(body): ApiJson<UpdateCampaignGroupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let mut group = for_record_write(&state, store, active.caller(), &audit, &group_id).await?;
    check_name(&body.name)?;
    let changed = group.rename(body.name, Timestamp::now());
    if changed {
        let written = store
            .update_group(&group)
            .await
            .map_err(|e| AppError::internal(e.context("failed to write the campaign group")))?;
        if !written {
            return Err(AppError::NotFound(format!("no campaign group {group_id}")));
        }
        tracing::info!(%group_id, "campaign group renamed");
    }
    audit.changed(changed);
    Ok(Json(Changed {
        record: pipette_plan_types::CampaignGroup::from(group),
        changed,
    }))
}

// DELETE /api/v1/campaign-groups/{group_id}
//
// Releases the members and keeps every campaign.
pub async fn delete_campaign_group(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    for_record_write(&state, store, active.caller(), &audit, &group_id).await?;
    store
        .delete_group(&group_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to delete the campaign group")))?;
    tracing::info!(%group_id, "campaign group deleted");
    Ok(StatusCode::NO_CONTENT)
}

// POST /api/v1/campaign-groups/{group_id}/members
//
// All or nothing: every campaign is checked before any joins. While a policy
// that re-runs or marks is on, joining takes a re-run's grants, as leaving one
// on does.
pub async fn add_campaign_group_members(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
    ApiJson(body): ApiJson<AddCampaignGroupMembersRequest>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let caller = active.caller();
    let (authority, group) = for_member_write(&state, store, caller, &audit, &group_id).await?;
    if stored_policies(store, &group_id)
        .await?
        .iter()
        .any(|policy| policy.enabled && policy.kind.reruns_or_marks())
    {
        require_rerun_grants(caller, &authority, &group)?;
    }
    let campaign_ids: BTreeSet<CampaignId> = body.campaign_ids.into_iter().collect();
    if campaign_ids.is_empty() {
        return Err(AppError::BadRequest(
            "campaign_ids is empty: name at least one campaign".into(),
        ));
    }
    let named = campaign_ids
        .iter()
        .map(|campaign_id| AuditTarget::Campaign {
            campaign_id: campaign_id.clone(),
        });
    audit.involve(&state, named).await;
    let (state, authority, group) = (&state, &authority, &group);
    stream::iter(campaign_ids.iter().cloned())
        .map(|campaign_id| joinable(state, authority, group, campaign_id))
        .buffered(STORAGE_CONCURRENCY)
        .try_collect::<Vec<()>>()
        .await?;
    let members: BTreeSet<CampaignId> = store
        .members(&group_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the group's members")))?
        .into_iter()
        .collect();
    let joining: Vec<CampaignId> = campaign_ids.difference(&members).cloned().collect();
    audit.changed(!joining.is_empty());
    if joining.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    store
        .add_members(&group_id, &joining)
        .await
        .map_err(|e| match e.downcast::<CampaignInAnotherGroupError>() {
            Ok(taken) => AppError::Conflict(taken.to_string()),
            Err(e) => AppError::internal(e.context("failed to add the campaigns to the group")),
        })?;
    tracing::info!(%group_id, campaigns = joining.len(), "campaigns joined the campaign group");
    Ok(StatusCode::NO_CONTENT)
}

async fn joinable(
    state: &AppState,
    authority: &CampaignAuthority,
    group: &CampaignGroup,
    campaign_id: CampaignId,
) -> Result<(), AppError> {
    let campaign = require(state, &campaign_id).await?;
    authority.require_run(&campaign)?;
    if campaign.owner == group.owner {
        return Ok(());
    }
    tracing::warn!(%campaign_id, owner = %campaign.owner.label(), "refused: the campaign has another owner than its group");
    Err(AppError::Conflict(format!(
        "campaign {campaign_id} belongs to {}, not to the group's owner {}",
        campaign.owner.label(),
        group.owner.label()
    )))
}

// DELETE /api/v1/campaign-groups/{group_id}/members/{campaign_id}
pub async fn remove_campaign_group_member(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath((Parsed(group_id), Parsed(campaign_id))): ApiPath<(
        Parsed<CampaignGroupId>,
        Parsed<CampaignId>,
    )>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    for_member_write(&state, store, active.caller(), &audit, &group_id).await?;
    let member = store
        .group_of(&campaign_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the campaign's group")))?
        .is_some_and(|of| of == group_id);
    if member {
        store
            .remove_member(&group_id, &campaign_id)
            .await
            .map_err(|e| AppError::internal(e.context("failed to remove the member")))?;
        tracing::info!(%group_id, %campaign_id, "campaign left the campaign group");
    }
    audit.changed(member);
    Ok(StatusCode::NO_CONTENT)
}

async fn stored_policies(
    store: &dyn CampaignGroupStore,
    group_id: &CampaignGroupId,
) -> Result<Vec<CampaignGroupPolicy>, AppError> {
    store
        .list_policies(group_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the group's policies")))
}

// GET /api/v1/campaign-groups/{group_id}/policies
pub async fn list_campaign_group_policies(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    for_read(&state, store, active.caller(), &group_id).await?;
    let stored = stored_policies(store, &group_id).await?;
    let policies = PolicyKind::iter()
        .map(|kind| kind.policy(&group_id, &stored))
        .collect();
    Ok(Json(CampaignGroupPolicies { policies }))
}

/// The group a read of its members' plans names, if the caller reads its
/// owner's campaigns, and the members whose plans the caller reads.
async fn plan_readers_members(
    state: &AppState,
    store: &dyn CampaignGroupStore,
    caller: Caller<'_>,
    group_id: &CampaignGroupId,
) -> Result<(CampaignGroup, Vec<Campaign>), AppError> {
    let (_, group) = for_read(state, store, caller, group_id).await?;
    // The members' plans and rows take the plan routes' reach: the group's is
    // wider by `manage_any_orgs` and an administrator's offboarded members.
    let org = user_org(state, &caller.user.user_id).await?;
    let reach = CampaignReach::over_plans(caller.user, caller.rights, org)?;
    let campaigns = readable_members(state, store, &reach, group_id).await?;
    Ok((group, campaigns))
}

// GET /api/v1/campaign-groups/{group_id}/policy-preview
//
// Members are read one after another, each with up to 16 pipette-duckdb row
// reads; after the first failure or timeout no later member's rows are read.
pub async fn preview_campaign_group_policies(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let db = state
        .results_db
        .as_ref()
        .ok_or_else(|| AppError::NotFound(NEEDS_DUCKDB.into()))?;
    let (group, campaigns) =
        plan_readers_members(&state, store, active.caller(), &group_id).await?;
    let stored = stored_policies(store, &group_id).await?;
    let mut states: BTreeMap<CampaignId, _> = store
        .policy_states(&group_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the group's members")))?
        .into_iter()
        .map(|member| (member.campaign_id, member.policy_state))
        .collect();
    let kept: Vec<(&Campaign, GroupPolicyState)> = campaigns
        .iter()
        .map(|campaign| match states.remove(&campaign.campaign_id) {
            Some(Err(bad)) => Err(AppError::internal(
                anyhow::Error::from(bad).context("a member's policy state does not decode"),
            )),
            Some(Ok(state)) => Ok((campaign, state.unwrap_or_default())),
            None => Ok((campaign, GroupPolicyState::default())),
        })
        .collect::<Result<_, _>>()?;
    let table = ModelParamTable::load(state.model_params_store.as_ref())
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the model params table")))?;
    let catalog = load_server_catalog(state.catalog_store.as_ref())
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the benchmark catalog")))?;
    let (plans, todo, submissions, group, stored, table, catalog, now) = (
        &*state.plan_store,
        &*state.todo_store,
        &*state.submission_store,
        &group,
        &stored,
        &table,
        &catalog,
        Timestamp::now(),
    );
    let (_, members) = stream::iter(kept.iter().map(anyhow::Ok))
        .try_fold(
            (Some(db), Vec::new()),
            |(results, mut members), (campaign, kept)| async move {
                let pass = Pass {
                    state: kept,
                    todo,
                    submissions,
                    preview: true,
                    table: Some(table),
                    catalog: Some(catalog),
                    waiting: &[],
                    job_states: &[],
                    bodies: None,
                };
                let evaluation =
                    group_policies::evaluate(plans, results, group, campaign, stored, pass).await?;
                let results = results.filter(|_| evaluation.state != PolicyMemberState::Unreadable);
                members.push(member_preview(campaign, kept, evaluation, now));
                anyhow::Ok((results, members))
            },
        )
        .await
        .map_err(|e| AppError::internal(e.context("failed to read a member's plan")))?;
    Ok(Json(CampaignGroupPolicyPreview { members }))
}

fn member_preview(
    campaign: &Campaign,
    kept: &GroupPolicyState,
    evaluation: Evaluation,
    now: Timestamp,
) -> MemberPolicyPreview {
    let Evaluation {
        plan_id,
        state,
        checks,
        answered_job_count,
        waiting_job_count,
        rerun_job_ids,
        marked,
        kept_job_count,
        skipped,
        earliest_deadline,
        hold,
        missing_model_params,
        smaller_failed,
        // The policy pass files, keeps and records these; the preview does
        // none of it.
        kinds: _,
        reruns: _,
        from_body: _,
        body_reads: _,
        judged: _,
        decided: _,
        flagged: _,
        read: _,
        job_states: _,
        record: _,
    } = evaluation;
    let earliest_deadline = earliest_deadline.and_then(ExpiresAt::instant);
    let (filing_plan_id, unclaimed_job_count) = match hold {
        Some(Hold::Queued { plan_id, waiting }) => (Some(plan_id), waiting),
        Some(Hold::NotAdvanced { plan_id, .. }) => (Some(plan_id), 0),
        Some(Hold::Refused(_)) | None => (None, 0),
    };
    MemberPolicyPreview {
        campaign_id: campaign.campaign_id.clone(),
        plan_id,
        state,
        answered_job_count,
        waiting_job_count,
        earliest_deadline,
        // The ingest refuses a deadline at or before now.
        deadline_passed: earliest_deadline.is_some_and(|deadline| deadline <= now),
        checks,
        rerun_job_ids,
        marked_job_ids: marked.into_keys().collect(),
        kept_job_count,
        skipped,
        filing_plan_id,
        unclaimed_job_count,
        missing_model_params,
        smaller_failed,
        last_filing: kept.filed.as_ref().map(|filed| PolicyFiling {
            plan_id: filed.plan_id.clone(),
            filed_at: filed.filed_at,
            queued_job_count: filed.queued.len(),
        }),
    }
}

// GET /api/v1/campaign-groups/{group_id}/rerun-results
//
// Reads what queue-maintenance recorded, and no pipette-duckdb.
pub async fn list_campaign_group_rerun_results(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(group_id)): ApiPath<Parsed<CampaignGroupId>>,
    ApiQuery(query): ApiQuery<RerunResultsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let (_, campaigns) = plan_readers_members(&state, store, active.caller(), &group_id).await?;
    let campaign_ids: Vec<CampaignId> = campaigns
        .into_iter()
        .map(|campaign| campaign.campaign_id)
        .collect();
    let window = JobWindow::of(query.all, query.offset, query.limit);
    let (offset, limit) = match window {
        JobWindow::Page { offset, limit } => (offset, Some(limit)),
        JobWindow::All => (0, None),
    };
    let (summary, rows) = store
        .rerun_results(&group_id, &campaign_ids, offset, limit)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the group's re-run results")))?;
    let total = summary.iter().map(|kind| kind.reruns).sum();
    let (offset, limit) = window.bounds(total);
    Ok(Json(CampaignGroupRerunResults {
        summary,
        total,
        offset,
        limit,
        rows,
    }))
}

// PUT /api/v1/campaign-groups/{group_id}/policies/{kind}
//
// A Turn off lands on whatever is stored, so a policy can always be stopped;
// any other write lands only on the policy its `base` read. A policy that
// re-runs or marks will re-run the owner's campaigns while it is on, so leaving
// one on takes a re-run's grants.
pub async fn set_campaign_group_policy(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath((Parsed(group_id), kind)): ApiPath<(Parsed<CampaignGroupId>, PolicyKind)>,
    ApiJson(body): ApiJson<SetCampaignGroupPolicyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let store = groups(&state)?;
    let caller = active.caller();
    let (authority, group) = for_member_write(&state, store, caller, &audit, &group_id).await?;
    let SetCampaignGroupPolicyRequest {
        enabled,
        max_reruns,
        settings,
        base,
    } = body;
    let stored = kind.policy(&group_id, &stored_policies(store, &group_id).await?);
    // From `base`, not the stored policy: Save changes on a policy read as off
    // must not turn off one another writer has turned on since.
    let (policy, changed) = if !enabled && base.enabled {
        turned_off(store, stored).await?
    } else {
        if enabled && kind.reruns_or_marks() {
            require_rerun_grants(caller, &authority, &group)?;
        }
        let (least, most) = (kind.min_reruns(), kind.max_reruns());
        if !(least..=most).contains(&max_reruns) {
            let takes = if least == most {
                least.to_string()
            } else {
                format!("{least} to {most}")
            };
            return Err(AppError::BadRequest(format!(
                "max_reruns is {max_reruns}: a {kind} policy takes {takes} re-runs"
            )));
        }
        kind.check_settings(&settings)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        let read = PolicyBase {
            enabled: stored.enabled,
            max_reruns: stored.max_reruns,
            settings: stored.settings.clone(),
        };
        let (enabled_by, enabled_at) = match (enabled, stored.enabled) {
            (true, false) => (Some(active.user.user_id.clone()), Some(Timestamp::now())),
            (true, true) => (stored.enabled_by.clone(), stored.enabled_at),
            (false, _) => (None, None),
        };
        let policy = CampaignGroupPolicy {
            group_id: group_id.clone(),
            kind,
            enabled,
            max_reruns,
            settings,
            enabled_by,
            enabled_at,
        };
        let changed = policy != stored;
        if base != read || (changed && !replaced(store, &stored, &policy).await?) {
            tracing::warn!(%group_id, %kind, "refused: the policy changed since it was read");
            return Err(AppError::Conflict(format!(
                "the {kind} policy changed since it was read; read it again"
            )));
        }
        (policy, changed)
    };
    audit.changed(changed);
    if changed {
        tracing::info!(%group_id, %kind, enabled = policy.enabled, "campaign group policy written");
    }
    Ok(Json(Changed {
        record: policy,
        changed,
    }))
}

/// `stored` turned off with its re-runs and settings, or left as it is once
/// found off. A write that meets another reads the policy again,
/// [`REPLACE_ATTEMPTS`] tries in all.
async fn turned_off(
    store: &dyn CampaignGroupStore,
    mut stored: CampaignGroupPolicy,
) -> Result<(CampaignGroupPolicy, bool), AppError> {
    let mut tries = 0;
    while stored.enabled {
        if tries == REPLACE_ATTEMPTS {
            tracing::warn!(
                group_id = %stored.group_id,
                kind = %stored.kind,
                "refused: the policy changed on every try to turn it off"
            );
            return Err(AppError::Conflict(format!(
                "the {} policy changed during each of {REPLACE_ATTEMPTS} tries to turn it off; \
                 nothing was written, retry",
                stored.kind
            )));
        }
        tries += 1;
        let off = CampaignGroupPolicy {
            enabled: false,
            enabled_by: None,
            enabled_at: None,
            ..stored.clone()
        };
        if replaced(store, &stored, &off).await? {
            return Ok((off, true));
        }
        stored = stored.kind.policy(
            &stored.group_id,
            &stored_policies(store, &stored.group_id).await?,
        );
    }
    Ok((stored, false))
}

async fn replaced(
    store: &dyn CampaignGroupStore,
    read: &CampaignGroupPolicy,
    updated: &CampaignGroupPolicy,
) -> Result<bool, AppError> {
    store
        .replace_policy(read, updated)
        .await
        .map_err(|e| AppError::internal(e.context("failed to write the policy")))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Context;

    use pipette_core::campaign::JobPolicyState;
    use pipette_core::types::Mint;
    use pipette_plan_types::{PolicyRerun, PolicyRerunSummary, UserId};
    use pipette_store::{CampaignGroupPage, EnabledPolicies, GroupMember, JobStateChanges};

    use super::*;

    /// A store on which every policy replace loses to another write. Its policy
    /// reads answer `reads` in turn, then the last one again.
    struct Contended {
        reads: Vec<CampaignGroupPolicy>,
        read: AtomicUsize,
        replaces: AtomicUsize,
    }

    impl Contended {
        fn new(reads: Vec<CampaignGroupPolicy>) -> Self {
            Self {
                reads,
                read: AtomicUsize::new(0),
                replaces: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl CampaignGroupStore for Contended {
        async fn create_group(&self, _: &CampaignGroup) -> anyhow::Result<()> {
            anyhow::bail!("create_group: not implemented by this fake")
        }
        async fn update_group(&self, _: &CampaignGroup) -> anyhow::Result<bool> {
            anyhow::bail!("update_group: not implemented by this fake")
        }
        async fn get_group(&self, _: &CampaignGroupId) -> anyhow::Result<Option<CampaignGroup>> {
            anyhow::bail!("get_group: not implemented by this fake")
        }
        async fn list_groups(
            &self,
            _: Option<&CampaignGroupId>,
            _: NonZeroUsize,
        ) -> anyhow::Result<CampaignGroupPage> {
            anyhow::bail!("list_groups: not implemented by this fake")
        }
        async fn delete_group(&self, _: &CampaignGroupId) -> anyhow::Result<()> {
            anyhow::bail!("delete_group: not implemented by this fake")
        }
        async fn add_members(&self, _: &CampaignGroupId, _: &[CampaignId]) -> anyhow::Result<()> {
            anyhow::bail!("add_members: not implemented by this fake")
        }
        async fn remove_member(&self, _: &CampaignGroupId, _: &CampaignId) -> anyhow::Result<()> {
            anyhow::bail!("remove_member: not implemented by this fake")
        }
        async fn members(&self, _: &CampaignGroupId) -> anyhow::Result<Vec<CampaignId>> {
            anyhow::bail!("members: not implemented by this fake")
        }
        async fn group_of(&self, _: &CampaignId) -> anyhow::Result<Option<CampaignGroupId>> {
            anyhow::bail!("group_of: not implemented by this fake")
        }
        async fn list_policies(
            &self,
            _: &CampaignGroupId,
        ) -> anyhow::Result<Vec<CampaignGroupPolicy>> {
            let n = self.read.fetch_add(1, Ordering::SeqCst);
            let policy = self.reads.get(n).or_else(|| self.reads.last());
            Ok(vec![policy.context("a policy to read")?.clone()])
        }
        async fn replace_policy(
            &self,
            _: &CampaignGroupPolicy,
            _: &CampaignGroupPolicy,
        ) -> anyhow::Result<bool> {
            self.replaces.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }
        async fn enabled_policies(&self) -> anyhow::Result<EnabledPolicies> {
            anyhow::bail!("enabled_policies: not implemented by this fake")
        }
        async fn policy_states(&self, _: &CampaignGroupId) -> anyhow::Result<Vec<GroupMember>> {
            anyhow::bail!("policy_states: not implemented by this fake")
        }
        async fn member_states(&self, _: &[CampaignId]) -> anyhow::Result<Vec<GroupMember>> {
            anyhow::bail!("member_states: not implemented by this fake")
        }
        async fn set_policy_state(
            &self,
            _: &CampaignGroupId,
            _: &CampaignId,
            _: &GroupPolicyState,
            _: &JobStateChanges,
        ) -> anyhow::Result<bool> {
            anyhow::bail!("set_policy_state: not implemented by this fake")
        }
        async fn job_states(
            &self,
            _: &CampaignGroupId,
            _: &CampaignId,
        ) -> anyhow::Result<Vec<JobPolicyState>> {
            anyhow::bail!("job_states: not implemented by this fake")
        }
        async fn record_reruns(
            &self,
            _: &CampaignGroupId,
            _: &[PolicyRerun],
        ) -> anyhow::Result<usize> {
            anyhow::bail!("record_reruns: not implemented by this fake")
        }
        async fn waiting_reruns(
            &self,
            _: &CampaignGroupId,
            _: &CampaignId,
        ) -> anyhow::Result<Vec<PolicyRerun>> {
            anyhow::bail!("waiting_reruns: not implemented by this fake")
        }
        async fn judge_reruns(
            &self,
            _: &CampaignGroupId,
            _: &[PolicyRerun],
        ) -> anyhow::Result<usize> {
            anyhow::bail!("judge_reruns: not implemented by this fake")
        }
        async fn rerun_results(
            &self,
            _: &CampaignGroupId,
            _: &[CampaignId],
            _: usize,
            _: Option<usize>,
        ) -> anyhow::Result<(Vec<PolicyRerunSummary>, Vec<PolicyRerun>)> {
            anyhow::bail!("rerun_results: not implemented by this fake")
        }
    }

    /// A Turn off that loses every replace answers `409` after
    /// [`REPLACE_ATTEMPTS`] tries, and one that reads the policy off again
    /// leaves it as it is.
    #[tokio::test]
    async fn a_turn_off_reads_again_after_a_lost_replace() -> anyhow::Result<()> {
        let off = PolicyKind::Swap.defaults(CampaignGroupId::mint());
        let on = CampaignGroupPolicy {
            enabled: true,
            enabled_by: Some(UserId::mint()),
            enabled_at: Some(Timestamp::now()),
            ..off.clone()
        };

        let store = Contended::new(vec![on.clone()]);
        let refused = turned_off(&store, on.clone()).await;
        assert!(matches!(refused, Err(AppError::Conflict(_))), "{refused:?}");
        assert_eq!(store.replaces.load(Ordering::SeqCst), REPLACE_ATTEMPTS);

        let store = Contended::new(vec![on.clone(), off.clone()]);
        assert_eq!(turned_off(&store, on).await?, (off, false));
        assert_eq!(store.replaces.load(Ordering::SeqCst), 2);
        Ok(())
    }
}
