//! The campaign reads: the listing, the one campaign it links into, and the
//! plans that campaign has run. The writes are [`crate::api_v1::campaigns`].

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;

use pipette_core::campaign::Campaign;
use pipette_core::plan::PlanManifest;
use pipette_pipeline::campaign_chain;
use pipette_plan_types::{
    CampaignId, CampaignListQuery, CampaignOverview, CampaignOverviewQuery, CampaignPlanRow,
    CampaignPlans, CampaignReadQuery, CampaignView, DeviceTimeTotal, Highlights, ListPage, Named,
    OrgId, OwnedListQuery, OwnerFilter, OwnerLookupQuery, PlanId, PlanReadQuery, PlanSummary,
    UserId, UserPermission,
};
use pipette_store::drain_campaigns;

use crate::api_v1::campaigns::{CampaignReviser, require};
use crate::auth::{ActiveUser, Caller, user_org};
use crate::campaign_access::{CampaignAuthority, CampaignReach};
use crate::error::AppError;
use crate::extract::{ApiPath, ApiQuery, Parsed};
use crate::list::list_limit;
use crate::names::{owner_labels, submitter_addresses};
use crate::owner::{OwnedPage, admits_filter, page_within_reach};
use crate::owner_choices::{boundary_of, campaign_choices};
use crate::state::AppState;

// GET /api/v1/campaigns
pub async fn list_campaigns(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiQuery(query): ApiQuery<OwnedListQuery>,
    ApiQuery(read): ApiQuery<CampaignReadQuery>,
    ApiQuery(filter): ApiQuery<CampaignListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let user = caller.user;
    let reach = CampaignReach::over_records(&state, caller).await?;
    let OwnedPage { page, drained } = match &filter.plan_id {
        Some(plan_id) => {
            tracing::Span::current().record("plan_id", tracing::field::display(&plan_id));
            // Refused as the paged listing refuses them, rather than ignored.
            list_limit(query.limit, &state.config)?;
            if query.cursor.is_some() {
                return Err(AppError::BadRequest(
                    "`plan_id` answers one page, so it takes no `cursor`".into(),
                ));
            }
            OwnedPage {
                page: filed_into(&state, plan_id, &query, &filter, &user.user_id, &reach).await?,
                drained: None,
            }
        }
        None => {
            page_within_reach(
                &state.config,
                query,
                &user.user_id,
                |campaign: &Campaign| reach.admits(campaign) && in_state(&filter, campaign),
                "campaigns",
                |after, limit| {
                    let store = state.campaign_store.clone();
                    async move { store.list_campaigns(after.as_ref(), limit).await }
                },
            )
            .await?
        }
    };
    let mut plans = if read.plan.unwrap_or(false) {
        let heads = page
            .items
            .iter()
            .filter_map(|campaign| campaign.current_plan_id.clone())
            .collect();
        // A newest-first page judges the scope off the drain it already read; an
        // ascending page reads the heads' campaigns.
        crate::api_v1::plans::reads::head_statuses(&state, caller, drained.as_deref(), heads)
            .await?
    } else {
        HashMap::new()
    };
    let names = if read.names.unwrap_or(false) {
        Some(owner_labels(&state, page.items.iter().map(|campaign| &campaign.owner)).await)
    } else {
        None
    };
    let items = page
        .items
        .into_iter()
        .map(|campaign| CampaignView {
            plan: campaign
                .current_plan_id
                .as_ref()
                .and_then(|plan_id| plans.remove(plan_id)),
            campaign: campaign.into(),
        })
        .collect();
    Ok(Json(Named {
        body: ListPage {
            items,
            next_cursor: page.next_cursor,
            unreadable: page.unreadable,
        },
        names,
    }))
}

/// Whether `campaign` is in the state `filter` asks for; any state without one.
fn in_state(filter: &CampaignListQuery, campaign: &Campaign) -> bool {
    filter.state.is_none_or(|state| campaign.state == state)
}

/// The campaign `plan_id`'s manifest names, so a superseded plan finds it
/// without a walk; empty, not refused, for no such plan or one outside the filters.
async fn filed_into(
    state: &AppState,
    plan_id: &PlanId,
    query: &OwnedListQuery,
    filter: &CampaignListQuery,
    caller: &UserId,
    reach: &CampaignReach,
) -> Result<ListPage<Campaign>, AppError> {
    let manifest = state
        .plan_store
        .get_plan(plan_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the plan manifest")))?;
    let campaign = match manifest.and_then(|manifest| manifest.campaign_id) {
        Some(campaign_id) => state
            .campaign_store
            .get_campaign(&campaign_id)
            .await
            .map_err(|e| AppError::internal(e.context("failed to read the campaign")))?,
        None => None,
    };
    Ok(ListPage::whole(
        campaign
            .into_iter()
            .filter(|campaign| {
                reach.admits(campaign)
                    && admits_filter(query.owner, query.org_id.as_ref(), &campaign.owner, caller)
                    && in_state(filter, campaign)
            })
            .collect(),
        0,
    ))
}

/// The campaign `campaign_id` names, refused outside the caller's reach.
pub(crate) async fn for_read(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<Campaign, AppError> {
    let reach = CampaignReach::over_records(state, caller).await?;
    let campaign = require(state, campaign_id).await?;
    reach.require(&campaign)?;
    Ok(campaign)
}

// GET /api/v1/campaigns/{campaign_id}
pub async fn get_campaign(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(read): ApiQuery<CampaignReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let campaign = for_read(&state, caller, &campaign_id).await?;
    let plan = match &campaign.current_plan_id {
        Some(head) if read.plan.unwrap_or(false) => {
            crate::api_v1::plans::reads::head_statuses(&state, caller, None, vec![head.clone()])
                .await?
                .remove(head)
        }
        _ => None,
    };
    let names = if read.names.unwrap_or(false) {
        Some(owner_labels(&state, std::iter::once(&campaign.owner)).await)
    } else {
        None
    };
    Ok(Json(Named {
        body: CampaignView {
            campaign: campaign.into(),
            plan,
        },
        names,
    }))
}

// GET /api/v1/campaigns/{campaign_id}/overview
pub async fn get_campaign_overview(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(read): ApiQuery<CampaignOverviewQuery>,
) -> Result<Json<Named<CampaignOverview>>, AppError> {
    let caller = active.caller();
    let campaign = for_read(&state, caller, &campaign_id).await?;
    let with_device_time = read.device_time.unwrap_or(false);
    Ok(Json(
        overview(&state, caller, campaign, with_device_time).await?,
    ))
}

// GET /api/v1/campaigns/{campaign_id}/plan
//
// One request, so a caller cannot see the pointer move between reading the
// campaign and reading its plan.
pub async fn get_campaign_plan(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<PlanReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let plan_id = current_plan(&state, caller, &campaign_id).await?;
    // Through the caller's plan scope, so the pointer answers what
    // `GET /api/v1/plans/{plan_id}` would.
    Ok(Json(
        crate::api_v1::plans::reads::status(&state, caller, &plan_id, &query).await?,
    ))
}

/// The plan a campaign the caller reaches points at.
async fn current_plan(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<PlanId, AppError> {
    let campaign = for_read(state, caller, campaign_id).await?;

    let plan_id = campaign.current_plan_id.ok_or_else(|| {
        // A `404` rather than an empty body, which would read as "here is its plan".
        AppError::NotFound(format!("campaign {campaign_id} points at no plan"))
    })?;
    tracing::Span::current().record("plan_id", tracing::field::display(&plan_id));
    Ok(plan_id)
}

// GET /api/v1/campaigns/{campaign_id}/plans
pub async fn list_campaign_plans(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let campaign = for_read(&state, caller, &campaign_id).await?;

    let chain = campaign_chain::walk(state.plan_store.as_ref(), &campaign)
        .await
        .map_err(|e| AppError::internal(e.context("failed to walk the campaign chain")))?;
    Ok(Json(CampaignPlans {
        current_plan_id: chain.plans.first().cloned(),
        superseded_plan_ids: chain.superseded().to_vec(),
        history_truncated: chain.truncated,
    }))
}

/// The organization's campaigns the caller reads, newest first, the first
/// `shown` as the listing's `plan=true` rows.
pub async fn org_campaigns(
    state: &AppState,
    caller: Caller<'_>,
    org_id: &OrgId,
    shown: usize,
) -> Result<Highlights<CampaignView>, AppError> {
    let reach = CampaignReach::over_records(state, caller).await?;
    // Drained rather than paged: the count needs every row judged. The keyspace
    // is operator-sized, which makes that affordable.
    let listing = drain_campaigns(&*state.campaign_store)
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the campaigns")))?;
    listing.unreadable.iter().for_each(|entry| {
        tracing::warn!(%entry, "unreadable campaign record in a listing");
    });
    // Newest first; the store returns creation order, so a reversal suffices.
    let campaigns = Highlights::of(
        listing.items.iter().rev().filter(|campaign| {
            reach.admits(campaign)
                && admits_filter(
                    Some(OwnerFilter::Org),
                    Some(org_id),
                    &campaign.owner,
                    &caller.user.user_id,
                )
        }),
        shown,
    );
    let heads = campaigns
        .first
        .iter()
        .filter_map(|campaign| campaign.current_plan_id.clone())
        .collect();
    let mut plans =
        crate::api_v1::plans::reads::head_statuses(state, caller, Some(&listing.items), heads)
            .await?;
    Ok(campaigns.map(|campaign| CampaignView {
        plan: campaign
            .current_plan_id
            .as_ref()
            .and_then(|plan_id| plans.remove(plan_id)),
        campaign: campaign.clone().into(),
    }))
}

/// Whether `user` may administer `campaign`'s record; advisory, the write routes
/// still decide. Only a `403` reads as `false`: swallowing a store failure would
/// tell an administrator the campaign is not theirs.
async fn may_administer(
    state: &AppState,
    caller: Caller<'_>,
    campaign: &Campaign,
) -> Result<bool, AppError> {
    match CampaignAuthority::resolve(state, caller).await {
        Ok(authority) => Ok(authority.may_administer(&campaign.owner)
            || authority
                .offboards(state, caller.rights, &campaign.owner)
                .await?),
        Err(AppError::Forbidden(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The organization where `caller` may create a descriptor catalog entry for
/// `campaign`'s descriptors: the owning organization, or the current
/// organization of the owning user. The caller must belong to it and hold
/// `edit_catalog`, or belong to no organization and hold `edit_any_catalog`.
/// This rule keeps the caller from reading the catalog names of another
/// organization. Advisory: the create route decides.
async fn catalog_org(
    state: &AppState,
    caller: Caller<'_>,
    campaign: &Campaign,
) -> Result<Option<OrgId>, AppError> {
    // `edit_any_catalog` implies `edit_catalog`, so one check covers both.
    if state.descriptor_catalog_store.is_none() || !caller.rights.holds(UserPermission::EditCatalog)
    {
        return Ok(None);
    }
    let target = boundary_of(state, &campaign.owner).await?;
    let own = user_org(state, &caller.user.user_id).await?;
    Ok(target.filter(|target| match &own {
        Some(own) => own == target,
        None => caller.rights.holds(UserPermission::EditAnyCatalog),
    }))
}

/// Whether a page drawing `plan_id` may offer to re-run its jobs: it is the
/// head of `campaign_id`, and a revision of it admits `user`. Advisory, not
/// enforced, as [`overview`]'s `may_edit_variants` is.
pub(crate) async fn may_rerun(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
    plan_id: &PlanId,
) -> Result<bool, AppError> {
    let campaign = state
        .campaign_store
        .get_campaign(campaign_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the campaign")))?;
    let Some(campaign) = campaign else {
        return Ok(false);
    };
    if campaign.current_plan_id.as_ref() != Some(plan_id) {
        return Ok(false);
    }
    // A head that recorded no digests carries nothing, so the route refuses it.
    let head = CampaignReviser::revisable_head(state, caller, &campaign).await?;
    Ok(head.is_some_and(|head| !head.content_keys.is_empty()))
}

/// One campaign with the plans it has run. `campaign` must come from [`for_read`],
/// as this checks no reach. A plan outside the reader's scope is drawn as withheld
/// (its id alone), not absent, so a running campaign never reads as idle.
pub(crate) async fn overview(
    state: &AppState,
    caller: Caller<'_>,
    campaign: Campaign,
    with_device_time: bool,
) -> Result<Named<CampaignOverview>, AppError> {
    let chain = campaign_chain::walk_links(state.plan_store.as_ref(), &campaign)
        .await
        .map_err(|e| AppError::internal(e.context("failed to walk the campaign chain")))?;
    let manifests: Vec<&PlanManifest> = chain
        .links
        .iter()
        .filter_map(|(_, manifest)| manifest.as_ref())
        .collect();
    let readable = crate::api_v1::plans::reads::readable(state, caller, &manifests).await?;
    let ids: Vec<PlanId> = manifests.iter().map(|m| m.plan_id.clone()).collect();
    let mut notes = state
        .plan_store
        .get_plan_notes_of(&ids)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the plans' notes")))?;
    // Only the last link can lack a manifest, so the links that have one are a
    // prefix and `readable` lines up with it.
    let links: Vec<(PlanId, Option<PlanSummary>)> = chain
        .links
        .into_iter()
        .zip(readable.into_iter().chain(std::iter::repeat(false)))
        .map(|((plan_id, manifest), readable)| {
            let plan = manifest.filter(|_| readable).map(|manifest| {
                let of = notes.remove(&manifest.plan_id).unwrap_or_default();
                manifest.summary(of, false)
            });
            (plan_id, plan)
        })
        .collect();

    let head_readable =
        campaign.current_plan_id.is_some() && links.first().is_some_and(|(_, plan)| plan.is_some());
    let may_administer = may_administer(state, caller, &campaign).await?;
    let catalog_org_id = catalog_org(state, caller, &campaign).await?;
    let revisable = if head_readable {
        CampaignReviser::revisable_head(state, caller, &campaign).await?
    } else {
        None
    };

    let submitters = links
        .iter()
        .filter_map(|(_, plan)| plan.as_ref()?.submitted_by.clone());
    let (current_plan, all_plans) = if with_device_time {
        device_time_scopes(&links, chain.truncated)
    } else {
        (None, None)
    };
    let (mut names, submitters, device_time_current_plan, device_time_all_plans) = futures::join!(
        owner_labels(state, std::iter::once(&campaign.owner)),
        submitter_addresses(state, submitters),
        device_time(
            state,
            &campaign.campaign_id,
            current_plan,
            Scope::CurrentPlan
        ),
        device_time(state, &campaign.campaign_id, all_plans, Scope::AllPlans),
    );
    names.extend(submitters);

    Ok(Named {
        body: CampaignOverview {
            campaign: campaign.into(),
            may_administer,
            may_edit_variants: revisable.is_some(),
            // [`may_rerun`]'s rule, from the reads above.
            may_rerun: revisable.is_some_and(|head| !head.content_keys.is_empty()),
            catalog_org_id,
            history: links
                .into_iter()
                .map(|(plan_id, plan)| CampaignPlanRow { plan_id, plan })
                .collect(),
            history_truncated: chain.truncated,
            device_time_current_plan,
            device_time_all_plans,
        },
        // Always given, empty or not: the page draws the owner from it.
        names: Some(names),
    })
}

/// A scope is `None` unless the reader may read each of its plans; the
/// all-plans one also when the walk stopped at its ceiling and would leave some out.
fn device_time_scopes(
    history: &[(PlanId, Option<PlanSummary>)],
    truncated: bool,
) -> (Option<Vec<PlanId>>, Option<Vec<PlanId>>) {
    let current_plan = history
        .first()
        .filter(|(_, plan)| plan.is_some())
        .map(|(plan_id, _)| vec![plan_id.clone()]);
    let all_plans =
        (!truncated && !history.is_empty() && history.iter().all(|(_, plan)| plan.is_some()))
            .then(|| history.iter().map(|(plan_id, _)| plan_id.clone()).collect());
    (current_plan, all_plans)
}

/// Which device-time total a read is for.
#[derive(Clone, Copy)]
enum Scope {
    /// The jobs of the head, re-scored copies included.
    CurrentPlan,
    /// Each distinct job of `history`, re-scored copies left out.
    AllPlans,
}

impl Scope {
    const fn skip_rescored(self) -> bool {
        matches!(self, Self::AllPlans)
    }

    /// The name of the total in the log.
    const fn label(self) -> &'static str {
        match self {
            Self::CurrentPlan => "current_plan",
            Self::AllPlans => "all_plans",
        }
    }
}

/// The device time `plans` spent, or `None` without a `[duckdb]` or plans. A
/// failed read logs a warning and reads `None`, so the campaign still answers.
async fn device_time(
    state: &AppState,
    campaign_id: &CampaignId,
    plans: Option<Vec<PlanId>>,
    scope: Scope,
) -> Option<DeviceTimeTotal> {
    let (db, plans) = state.results_db.as_ref().zip(plans)?;
    match db.device_time_total(&plans, scope.skip_rescored()).await {
        Ok(total) => Some(total),
        Err(e) => {
            tracing::warn!(
                %campaign_id,
                scope = scope.label(),
                error = %format!("{:#}", anyhow::Error::from(e)),
                "could not read the campaign's device-time total from pipette-duckdb; the campaign answers without it"
            );
            None
        }
    }
}

// GET /api/v1/campaigns/{campaign_id}/owners
pub async fn list_campaign_owners(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<OwnerLookupQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let choices = campaign_choices(&state, caller, &campaign_id).await?;
    Ok(Json(choices.matching(&query.q, query.owner_type)))
}

#[cfg(test)]
mod tests {
    use pipette_core::plan::sample_manifest;
    use pipette_plan_types::PlanStatus;

    use super::*;

    /// A history of one row per entry, newest first, each readable or withheld.
    fn history(readable: &[bool]) -> anyhow::Result<Vec<(PlanId, Option<PlanSummary>)>> {
        readable
            .iter()
            .enumerate()
            .map(|(i, &readable)| {
                let plan_id =
                    PlanId::try_new(format!("plan-0192b3c4-0000-7000-8000-00000000000{i}"))?;
                let plan = readable
                    .then(|| sample_manifest(plan_id.clone(), PlanStatus::Active, false))
                    .transpose()?
                    .map(|manifest| manifest.summary(Default::default(), false));
                Ok((plan_id, plan))
            })
            .collect()
    }

    /// Each total covers its plans only when the reader reads every one, and
    /// the all-plans total only when the walk reached the first plan.
    #[rstest::rstest]
    #[case::every_plan_readable(&[true, true, true], false, Some(1), Some(3))]
    #[case::head_withheld(&[false, true], false, None, None)]
    #[case::older_plan_withheld(&[true, false, true], false, Some(1), None)]
    #[case::last_manifest_gone(&[true, true, false], false, Some(1), None)]
    #[case::history_truncated(&[true, true], true, Some(1), None)]
    #[case::empty_history(&[], false, None, None)]
    fn a_device_time_total_covers_only_plans_the_reader_reads_whole(
        #[case] readable: &[bool],
        #[case] truncated: bool,
        #[case] current_plan: Option<usize>,
        #[case] all_plans: Option<usize>,
    ) -> anyhow::Result<()> {
        let history = history(readable)?;
        let (current, all) = device_time_scopes(&history, truncated);
        let ids = |n: usize| -> Vec<PlanId> {
            history
                .iter()
                .take(n)
                .map(|(plan_id, _)| plan_id.clone())
                .collect()
        };
        assert_eq!(current, current_plan.map(ids));
        assert_eq!(all, all_plans.map(ids));
        Ok(())
    }
}
