//! A campaign's annotated plan tags: a name and a message on one plan the
//! campaign filed, made once and never changed.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use pipette_core::campaign::Campaign;
use pipette_pipeline::campaign_admin;
use pipette_plan_types::{CampaignId, CreatePlanTagRequest, PlanTag, PlanTags, UserPermission};

use crate::api_v1::campaigns::for_plan_write;
use crate::api_v1::campaigns::reads::for_read;
use crate::auth::{ActiveUser, Caller, user_org};
use crate::campaign_access::CampaignAuthority;
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, Parsed};
use crate::state::AppState;

// POST /api/v1/campaigns/{campaign_id}/tags
pub async fn create_plan_tag(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiJson(body): ApiJson<CreatePlanTagRequest>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    // A filing's gate, whatever the campaign's state: a tag moves no plan.
    for_plan_write(&state, caller, &campaign_id).await?;
    caller.rights.require(UserPermission::SubmitPlans)?;
    let tag = campaign_admin::tag_plan(
        state.plan_tag_store.as_deref(),
        state.plan_store.as_ref(),
        &campaign_id,
        body,
        Some(caller.user.user_id.clone()),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(tag)))
}

// GET /api/v1/campaigns/{campaign_id}/tags
pub async fn list_plan_tags(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
) -> Result<Json<PlanTags>, AppError> {
    for_read(&state, active.caller(), &campaign_id).await?;
    Ok(Json(PlanTags {
        tags: campaign_tags(&state, std::slice::from_ref(&campaign_id)).await?,
    }))
}

/// The tags of each of `campaign_ids`, newest first; none without a
/// `[database]`.
pub(crate) async fn campaign_tags(
    state: &AppState,
    campaign_ids: &[CampaignId],
) -> Result<Vec<PlanTag>, AppError> {
    let Some(store) = state.plan_tag_store.as_deref() else {
        return Ok(Vec::new());
    };
    store
        .campaign_tags(campaign_ids)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the plan tags")))
}

/// Whether `caller` may tag `campaign`'s plans: it may file into the campaign,
/// whatever its state, and the server keeps tags.
pub(crate) async fn may_tag(
    state: &AppState,
    caller: Caller<'_>,
    campaign: &Campaign,
) -> Result<bool, AppError> {
    if state.plan_tag_store.is_none() || !caller.rights.holds(UserPermission::SubmitPlans) {
        return Ok(false);
    }
    let member_of = user_org(state, &caller.user.user_id).await?;
    Ok(CampaignAuthority::of(caller.user, caller.rights, member_of).may_run(&campaign.owner))
}
