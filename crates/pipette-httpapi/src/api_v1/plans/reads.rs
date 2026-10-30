//! The plan read and control surface: the `/api/v1/plans` reads,
//! `GET /api/v1/jobs/{job_id}/plans` and `POST /api/v1/plans/{plan_id}/cancel`.
//! Routes take [`ActiveUser`], so an account's standing is checked before any
//! permission.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::Value;

use pipette_core::campaign::Campaign;
use pipette_core::plan::{
    PlanHeader, PlanManifest, PlanNotes, PlanSource, PlanSummary, SettledOutcome,
};
use pipette_core::todo_filename::{Lease, parse_leased_key};
use pipette_core::user::User;
use pipette_core::warehouse::is_system_client;
use pipette_pipeline::campaign_chain;
use pipette_pipeline::org_catalog;
use pipette_pipeline::plan_cells::{
    JobCells, PlanCell, filed_types, in_document_order, spawn_document_cells,
};
use pipette_pipeline::plan_cli::{CancelOutcome, explain_plan_with, request_cancel};
use pipette_pipeline::plan_document::SourceRead;
use pipette_pipeline::plan_results::{ResultRow, ResultsDb, ResultsError, rows_by_job};
use pipette_pipeline::queue_maintenance::{AvailKey, collect_all_avail};
use pipette_pipeline::submission::ServedJobSource;
use pipette_pipeline::variant_edit;
use pipette_pipeline::vault_check::{named_resources, typed_descriptor_sha256};
use pipette_plan_types::{
    BenchmarkId, CampaignId, Changed, ClientId, ClientRunSpec, DocumentState, FleetDetail, Job,
    JobId, JobInPlan, JobPlans, JobQueue, JobResult, JobState, ListOrder, Model, Named,
    NonEmptyTrimmedString, ParentJob, ParentRun, PlanCancelRequested, PlanId, PlanJob, PlanJobs,
    PlanJobsQuery, PlanListQuery, PlanReadQuery, PlanStatus, PlanVariants, PlanView,
    RequestedScope, ResultsRead, Runtime, UpdatePlanRequest, UserId, UserPermission,
    VaultResourceId,
};
use pipette_store::{PlanFilter, PlanReach, STORAGE_CONCURRENCY, drain_campaigns};

use super::csv::results_csv;
use super::job_results::{rescored_of, with_job_results};
use super::results_cache::PlanJobResults;
use crate::api_v1::model_params::fill_model_params;
use crate::api_v1::plans::check_note;
use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, UserRights, user_org};
use crate::campaign_access::{CampaignAuthority, CampaignReach};
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery, Parsed};
use crate::list::page_newest;
use crate::names::{descriptor_names, device_names, submitter_addresses, vault_names};
use crate::state::AppState;
use crate::vault_access::VaultReach;

/// Default page size for `GET /api/v1/plans/{plan_id}/jobs`, and its ceiling. The
/// page size is the store cost: a job that left the queue may take a `find_job`.
pub(crate) const JOBS_PAGE_DEFAULT: usize = 100;
pub(crate) const JOBS_PAGE_MAX: usize = 500;

/// The scope a read was resolved at, once per request, so no row is judged against
/// a set that moved. A campaign's plans follow its owner as that stands now.
enum PlanScope {
    /// `read_any_plans`: every plan.
    Any,
    /// `read_plans`: the plans of the campaigns `reach` admits, none of a
    /// deleted one's. A plan outside every campaign is its filer's; a
    /// campaign reader reads none.
    Reach(CampaignReach),
}

/// Which plans the caller may read, and how much fleet detail they disclose.
pub(crate) struct PlanRead {
    scope: PlanScope,
    fleet: FleetDetail,
    pub(crate) rights: UserRights,
}

impl PlanRead {
    async fn resolve(
        state: &AppState,
        caller: Caller<'_>,
        requested: Option<RequestedScope>,
    ) -> Result<Self, AppError> {
        caller.rights.require(UserPermission::ReadPlans)?;
        Self::from_rights(state, caller.user, caller.rights.clone(), requested).await
    }

    /// [`resolve`](Self::resolve) for a page that draws less without
    /// `read_plans`: `None`, and no refusal logged.
    async fn if_granted(state: &AppState, caller: Caller<'_>) -> Result<Option<Self>, AppError> {
        if !caller.rights.holds(UserPermission::ReadPlans) {
            return Ok(None);
        }
        Self::from_rights(state, caller.user, caller.rights.clone(), None)
            .await
            .map(Some)
    }

    /// How far the caller's plan reads reach, as a listing that names no
    /// scope takes it: what `GET /api/v1/users/me` answers.
    pub(crate) async fn reach(
        state: &AppState,
        caller: Caller<'_>,
    ) -> Result<pipette_plan_types::Reach, AppError> {
        use pipette_plan_types::Reach;
        Ok(match Self::if_granted(state, caller).await? {
            None => Reach::None,
            Some(Self {
                scope: PlanScope::Any,
                ..
            }) => Reach::Every,
            Some(Self {
                scope: PlanScope::Reach(CampaignReach::Listed { .. }),
                ..
            }) => Reach::Listed,
            Some(Self {
                scope: PlanScope::Reach(_),
                ..
            }) => Reach::Own,
        })
    }

    /// The scope `rights` resolve to, once they hold `read_plans`.
    async fn from_rights(
        state: &AppState,
        user: &User,
        rights: UserRights,
        requested: Option<RequestedScope>,
    ) -> Result<Self, AppError> {
        let scope = match requested {
            Some(RequestedScope::Mine) => PlanScope::Reach(CampaignReach::own(
                user,
                user_org(state, &user.user_id).await?,
            )),
            Some(RequestedScope::All) => {
                rights.require(UserPermission::ReadAnyPlans)?;
                PlanScope::Any
            }
            None if rights.holds(UserPermission::ReadAnyPlans) => PlanScope::Any,
            None => {
                let org = user_org(state, &user.user_id).await?;
                PlanScope::Reach(CampaignReach::over_plans(user, &rights, org)?)
            }
        };
        Ok(Self {
            scope,
            fleet: rights.fleet_detail(),
            rights,
        })
    }
}

impl PlanScope {
    /// Whether this scope admits `manifest`, reading the campaign it names only
    /// when the answer turns on it.
    async fn admits_one(
        &self,
        state: &AppState,
        manifest: &PlanManifest,
    ) -> Result<bool, AppError> {
        let named = manifest.campaign_id.iter().cloned().collect();
        let (reached, error) = self.reached_among(state, named).await;
        if let Some(error) = error {
            return Err(error);
        }
        Ok(reached.admits(
            manifest.campaign_id.as_ref(),
            manifest.submitted_by.as_ref(),
        ))
    }

    /// The campaigns among `named` this scope reaches, one read each; a failed
    /// read comes back beside the answer. Owned ids, for the handler's `Send` bound.
    async fn reached_among(
        &self,
        state: &AppState,
        named: HashSet<CampaignId>,
    ) -> (Reached, Option<AppError>) {
        let Self::Reach(reach) = self else {
            return (Reached::Every, None);
        };
        let reads: Vec<_> = stream::iter(named)
            .map(|campaign_id| async move {
                state
                    .campaign_store
                    .get_campaign(&campaign_id)
                    .await
                    .map_err(|e| {
                        AppError::internal(e.context("failed to read the plan's campaign"))
                    })
            })
            .buffer_unordered(STORAGE_CONCURRENCY)
            .collect()
            .await;
        let (campaigns, error) = reads.into_iter().fold(
            (BTreeSet::new(), None),
            |(mut reached, error), read| match read {
                Ok(Some(campaign)) if reach.admits(&campaign) => {
                    reached.insert(campaign.campaign_id);
                    (reached, error)
                }
                Err(e) => (reached, error.or(Some(e))),
                Ok(_) => (reached, error),
            },
        );
        let reach = PlanReach {
            campaigns,
            filer: reach.filer().cloned(),
        };
        (Reached::These(reach), error)
    }

    /// The campaigns this scope reaches, drained once per listing. A
    /// `CampaignReach::Any` drains too, so a deleted campaign's plans leave a
    /// listing as [`admits_one`](Self::admits_one) refuses them.
    async fn reached(&self, state: &AppState) -> Result<Reached, AppError> {
        if matches!(self, Self::Any) {
            return Ok(Reached::Every);
        }
        let listing = drain_campaigns(&*state.campaign_store)
            .await
            .map_err(|e| AppError::internal(e.context("failed to list the campaigns")))?;
        // An unreadable record is reached by nobody, as an absent one is.
        listing.unreadable.iter().for_each(|entry| {
            tracing::warn!(%entry, "unreadable campaign record under a plan scope");
        });
        Ok(self.reached_in(&listing.items))
    }

    /// [`reached`](Self::reached) over campaigns the caller already drained.
    fn reached_in(&self, campaigns: &[Campaign]) -> Reached {
        match self {
            Self::Any => Reached::Every,
            Self::Reach(reach) => Reached::These(PlanReach {
                campaigns: campaigns
                    .iter()
                    .filter(|campaign| reach.admits(campaign))
                    .map(|campaign| campaign.campaign_id.clone())
                    .collect(),
                filer: reach.filer().cloned(),
            }),
        }
    }
}

/// The plans a [`PlanScope`] reaches, its campaigns read, for a request that
/// judges many manifests.
enum Reached {
    /// `read_any_plans`: every plan, which takes no read to say.
    Every,
    These(PlanReach),
}

impl Reached {
    /// Whether a plan filed under `campaign_id` by `submitted_by` is reached,
    /// by the rule the store filters with.
    fn admits(&self, campaign_id: Option<&CampaignId>, submitted_by: Option<&UserId>) -> bool {
        match self {
            Self::Every => true,
            Self::These(reach) => reach.admits(campaign_id, submitted_by),
        }
    }

    /// As a store filter, so a listing's store pages hold the admitted plans
    /// alone. `None` narrows nothing.
    fn store_reach(&self) -> Option<PlanReach> {
        match self {
            Self::Every => None,
            Self::These(reach) => Some(reach.clone()),
        }
    }
}

// GET /api/v1/plans
pub async fn list_plans(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiQuery(query): ApiQuery<PlanListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let asked = query.names.unwrap_or(false);
    let page = api_listing(&state, caller, query).await?;
    // The submitters of the rows served, never the `submitted_by` filter's.
    let names = if asked {
        let submitters = page
            .items
            .iter()
            .filter_map(|plan| plan.submitted_by.clone());
        Some(submitter_addresses(&state, submitters).await)
    } else {
        None
    };
    Ok(Json(Named { body: page, names }))
}

async fn api_listing(
    state: &AppState,
    caller: Caller<'_>,
    query: PlanListQuery,
) -> Result<pipette_plan_types::ListPage<PlanView>, AppError> {
    let read = PlanRead::resolve(state, caller, query.scope).await?;
    let limit = crate::list::list_limit(query.limit, &state.config)?;
    let after = crate::list::list_cursor(query.cursor, PlanId::try_new)?;
    let reached = read.scope.reached(state).await?;
    let plan_store = state.plan_store.as_ref();
    let filter = PlanFilter {
        status: query.status,
        campaign_id: query.campaign_id,
        submitted_by: query.submitted_by,
        reach: reached.store_reach(),
    };
    let filter = &filter;
    // Every store filters by reach too; this re-check keeps a store bug from
    // leaking a plan.
    let admits =
        |plan: &PlanHeader| reached.admits(plan.campaign_id.as_ref(), plan.submitted_by.as_ref());
    let fetch = |after: Option<PlanId>, remaining| async move {
        plan_store
            .list_matching_plan_headers(filter, after.as_ref(), remaining)
            .await
    };
    let page = match query.order.unwrap_or_default() {
        ListOrder::Oldest => {
            crate::list::page_filtered(limit, after, "plans", admits, fetch).await?
        }
        ListOrder::Newest => {
            page_newest(limit, after, "plans", admits, plan_key, fetch)
                .await?
                .page
        }
    };

    let cancelled: HashSet<PlanId> = state
        .plan_store
        .list_cancel_markers()
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the pending cancel markers")))?
        .into_iter()
        .collect();
    Ok(pipette_plan_types::ListPage {
        items: page
            .items
            .into_iter()
            .map(|manifest| {
                let cancel_requested = cancelled.contains(&manifest.plan_id);
                manifest.view(cancel_requested, read.fleet)
            })
            .collect(),
        next_cursor: page.next_cursor,
        unreadable: page.unreadable,
    })
}

/// The key a plan lists under, which a newest-first cursor bounds.
fn plan_key(plan: &PlanHeader) -> &PlanId {
    &plan.plan_id
}

/// A job body's request, typed by its plan's [`filed_types`]; `None` when this
/// build does not read its spec, which costs the row its request alone.
fn body_request(job_id: &JobId, body: &Value, manifest: &PlanManifest) -> Option<PlanCell> {
    let spec = body.get("spec")?;
    match ClientRunSpec::deserialize(spec) {
        Ok(spec) => {
            let types = filed_types(&manifest.benchmark_types, [&spec.benchmark]);
            Some(PlanCell::of(&spec, &types))
        }
        Err(e) => {
            tracing::warn!(
                job_id = %job_id,
                error = %e,
                "a job's spec does not read; its row names no request"
            );
            None
        }
    }
}

/// [`body_request`] of each of `job_ids`' stored sources, read in one call. An
/// unparseable source reads as none, so one damaged object does not cost the
/// whole page.
async fn stored_requests(
    state: &AppState,
    manifest: &PlanManifest,
) -> Result<HashMap<JobId, PlanCell>, AppError> {
    let sources = state
        .plan_store
        .get_job_sources(&manifest.job_ids)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the job sources")))?;
    for unreadable in &sources.unreadable {
        tracing::warn!(
            key = %unreadable.key,
            error = %unreadable.reason,
            "a job source does not parse; its row names no request"
        );
    }
    Ok(sources
        .items
        .into_iter()
        .filter_map(|(job_id, body)| {
            Some((job_id.clone(), body_request(&job_id, &body, manifest)?))
        })
        .collect())
}

/// Where a run leaves its job: a run is scored from the job's record, so it
/// says what the record does.
fn run_state(rows: &[ResultRow]) -> JobState {
    if rows.iter().any(|row| row.failed == Some(true)) {
        JobState::Failed
    } else {
        JobState::Finished
    }
}

/// Where a job stands whose record `queue-maintenance` read for this plan: what
/// the plan's `progress_snapshot` counts it as, before its run is published.
fn settled_state(outcome: SettledOutcome) -> JobState {
    match outcome {
        SettledOutcome::Finished => JobState::Finished,
        SettledOutcome::Failed => JobState::Failed,
    }
}

/// Where a job out of both queues stands with no run and no recorded outcome.
/// With the runs read, a cancelled or replaced plan tore it down, and an active
/// plan holds it answered.
fn unsettled_state(read: ResultsRead, plan: PlanStatus) -> JobState {
    let torn_down = match plan {
        PlanStatus::Complete | PlanStatus::Cancelling | PlanStatus::Cancelled => true,
        PlanStatus::Creating | PlanStatus::Active | PlanStatus::PendingClients => false,
    };
    if torn_down && read == ResultsRead::Ok {
        JobState::Cancelled
    } else {
        JobState::Unknown
    }
}

/// Load the manifest `plan_id` names, if the caller's scope admits it. A plan
/// outside the scope reads as absent: a `403` would confirm the id is real.
async fn scoped_manifest(
    state: &AppState,
    scope: &PlanScope,
    plan_id: &PlanId,
) -> Result<PlanManifest, AppError> {
    let manifest = state
        .plan_store
        .get_plan(plan_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the plan manifest")))?
        .ok_or_else(|| AppError::NotFound("plan not found".into()))?;
    if !scope.admits_one(state, &manifest).await? {
        tracing::warn!(%plan_id, "refused: the plan is outside the caller's scope");
        return Err(AppError::OutOfScope("plan not found".into()));
    }
    Ok(manifest)
}

/// Whether the caller may read each of `manifests`, from one scope resolution
/// and one read per campaign they name. All `false` for a caller holding no
/// plan read grant, which is a state here rather than a refusal.
pub(crate) async fn readable(
    state: &AppState,
    caller: Caller<'_>,
    manifests: &[&PlanManifest],
) -> Result<Vec<bool>, AppError> {
    let Some(read) = PlanRead::if_granted(state, caller).await? else {
        return Ok(vec![false; manifests.len()]);
    };
    let named = manifests
        .iter()
        .filter_map(|manifest| manifest.campaign_id.clone())
        .collect();
    let (reached, error) = read.scope.reached_among(state, named).await;
    if let Some(error) = error {
        return Err(error);
    }
    Ok(manifests
        .iter()
        .map(|manifest| {
            reached.admits(
                manifest.campaign_id.as_ref(),
                manifest.submitted_by.as_ref(),
            )
        })
        .collect())
}

/// The row of each of `heads` the caller's scope admits, by plan id, judged
/// against `drained` when given: a head can belong to another campaign than the
/// one pointing at it.
pub(crate) async fn head_statuses(
    state: &AppState,
    caller: Caller<'_>,
    drained: Option<&[Campaign]>,
    heads: Vec<PlanId>,
) -> Result<HashMap<PlanId, pipette_plan_types::PlanSummary>, AppError> {
    if heads.is_empty() {
        return Ok(HashMap::new());
    }
    let Some(read) = PlanRead::if_granted(state, caller).await? else {
        return Ok(HashMap::new());
    };
    let (summaries, cancelled) = futures::join!(
        // A failed read fails the page, as the marker listing beside it does.
        async {
            state
                .plan_store
                .get_plan_summaries(&heads)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read the head plans")))
        },
        // One listing rather than a marker read per row, as `listing` does.
        async {
            state.plan_store.list_cancel_markers().await.map_err(|e| {
                AppError::internal(e.context("failed to list the pending cancel markers"))
            })
        },
    );
    let cancelled: HashSet<PlanId> = cancelled?.into_iter().collect();
    let summaries = summaries?;
    // A manifest that does not read costs its row its `plan`, not the listing
    // its page.
    summaries.unreadable.iter().for_each(|entry| {
        tracing::warn!(%entry, "a campaign's plan manifest did not read; its row shows the id alone");
    });
    let summaries = summaries.items;
    let reached = match drained {
        Some(campaigns) => read.scope.reached_in(campaigns),
        None => {
            let named = summaries
                .iter()
                .filter_map(|summary| summary.campaign_id.clone())
                .collect();
            let (reached, error) = read.scope.reached_among(state, named).await;
            if let Some(error) = error {
                return Err(error);
            }
            reached
        }
    };
    Ok(summaries
        .into_iter()
        .filter(|summary| {
            reached.admits(summary.campaign_id.as_ref(), summary.submitted_by.as_ref())
        })
        .map(|summary| {
            let cancel_requested = cancelled.contains(&summary.plan_id);
            (summary.plan_id.clone(), summary.view(cancel_requested))
        })
        .collect())
}

/// Resolve the scope, then the manifest, so a caller holding no read grant is
/// refused without learning whether the plan exists.
async fn read_plan(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
) -> Result<(PlanManifest, PlanRead), AppError> {
    let read = PlanRead::resolve(state, caller, None).await?;
    let manifest = scoped_manifest(state, &read.scope, plan_id).await?;
    Ok((manifest, read))
}

// GET /api/v1/plans/{plan_id}
pub async fn get_plan(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
    ApiQuery(query): ApiQuery<PlanReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        status(&state, active.caller(), &plan_id, &query).await?,
    ))
}

/// One plan's status view, scoped to what the caller may read, with its
/// submitter's address when `query` asks.
pub(crate) async fn status(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
    query: &PlanReadQuery,
) -> Result<Named<PlanView>, AppError> {
    // The scope resolves first, so a caller holding no read grant is refused
    // before the store is asked about the plan; then the manifest and its marker.
    let read = PlanRead::resolve(state, caller, None).await?;
    let (manifest, cancel_requested, notes) = futures::try_join!(
        scoped_manifest(state, &read.scope, plan_id),
        async {
            state
                .plan_store
                .has_cancel_marker(plan_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read the cancel marker")))
        },
        plan_notes(state, plan_id),
    )?;
    let plan = manifest.view(notes, cancel_requested, read.fleet);
    let names = if query.names.unwrap_or(false) {
        Some(submitter_addresses(state, plan.submitted_by.iter().cloned()).await)
    } else {
        None
    };
    Ok(Named { body: plan, names })
}

// GET /api/v1/plans/{plan_id}/document
pub async fn get_plan_document(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
) -> Result<Response, AppError> {
    let source = plan_document(&state, active.caller(), &plan_id).await?;
    Ok(toml_response(source.text().to_string()))
}

// GET /api/v1/plans/{plan_id}/variants
//
// TOML by default; `Accept: application/json` lists one block per variant, as a
// variant edit's `restore` numbers them.
pub async fn get_plan_variants(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let source = plan_document(&state, active.caller(), &plan_id).await?;
    let internal = |e| AppError::internal(anyhow::Error::new(e));
    let mut response = if crate::progress_stream::accepts(&headers, "application/json") {
        let variants = variant_edit::standalone_variant_blocks(source.text()).map_err(internal)?;
        Json(PlanVariants { variants }).into_response()
    } else {
        let blocks = variant_edit::standalone_blocks(source.text()).map_err(internal)?;
        toml_response(blocks)
    };
    response
        .headers_mut()
        .insert(VARY, HeaderValue::from_static("accept"));
    Ok(response)
}

fn toml_response(text: String) -> Response {
    ([(CONTENT_TYPE, "application/toml; charset=utf-8")], text).into_response()
}

/// The plan document `plan_id` was filed from, read under the plan scope.
async fn plan_document(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
) -> Result<PlanSource, AppError> {
    let (manifest, read) = read_plan(state, caller, plan_id).await?;
    // A plan filed as job bodies has no document to withhold, so it answers the
    // same `404` at both tiers, as the plan page draws it.
    if manifest.source_sha256.is_some() {
        crate::api_v1::plans::require_document_detail(read.fleet, plan_id)?;
    }
    match read_source(state, &manifest).await? {
        SourceRead::NoSource => Err(AppError::NotFound("plan has no source document".into())),
        SourceRead::Collected => Err(AppError::NotFound("plan source was collected".into())),
        SourceRead::Mismatch => Err(AppError::internal(anyhow::anyhow!(
            "the stored source document does not match the manifest's source_sha256"
        ))),
        SourceRead::Present(source) => Ok(source),
    }
}

// GET /api/v1/jobs/{job_id}/plans
pub async fn list_job_plans(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(job_id)): ApiPath<Parsed<JobId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    Ok(Json(job_plans(&state, caller, &job_id).await?))
}

/// The plans in the caller's scope that link `job_id`, oldest first and in every
/// status, and the shared job-source fields without their tokens.
pub(crate) async fn job_plans(
    state: &AppState,
    caller: Caller<'_>,
    job_id: &JobId,
) -> Result<JobPlans, AppError> {
    // The scope resolves before any store read, so a caller that holds no read
    // grant learns nothing about the job.
    let read = PlanRead::resolve(state, caller, None).await?;
    let links = state
        .plan_store
        .list_job_plan_links(job_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the job's plan links")))?;

    // One batched read, as a shared job can have hundreds of links. Failures wait
    // until after scope filtering, so a foreign record cannot tell a job from a miss.
    let (linked, read_error) = match state.plan_store.get_plan_summaries(&links).await {
        Ok(listing) => {
            // No code path deletes a manifest, so a link goes unread for a
            // manifest that does not read or that someone deleted by hand.
            let read: HashSet<&PlanId> = listing.items.iter().map(|plan| &plan.plan_id).collect();
            let unread: Vec<&PlanId> = links.iter().filter(|id| !read.contains(id)).collect();
            if !unread.is_empty() {
                tracing::debug!(
                    plan_ids = ?unread,
                    "skipping job-plan links whose manifest is gone or does not read"
                );
            }
            let error = listing.unreadable.into_iter().next().map(|entry| {
                AppError::internal(
                    anyhow::Error::new(entry).context("failed to read a linked plan manifest"),
                )
            });
            (listing.items, error)
        }
        Err(e) => (
            Vec::new(),
            Some(AppError::internal(
                e.context("failed to read the linked plan manifests"),
            )),
        ),
    };
    let (reached, campaign_error) = read
        .scope
        .reached_among(
            state,
            linked
                .iter()
                .filter_map(|plan| plan.campaign_id.clone())
                .collect(),
        )
        .await;
    let read_error = read_error.or(campaign_error);
    let any_linked = !linked.is_empty();
    let mut plans: Vec<PlanSummary> = linked
        .into_iter()
        .filter(|plan| reached.admits(plan.campaign_id.as_ref(), plan.submitted_by.as_ref()))
        .collect();

    // One answer for no link and for out-of-scope links only: a caller must not
    // learn that a job exists from a plan it cannot read.
    if plans.is_empty() {
        if let Some(error) = read_error {
            tracing::error!(%error, "no readable plan could be confirmed for a linked job");
        } else if any_linked {
            tracing::warn!("refused: every plan linking the job is outside the caller's scope");
        }
        return Err(AppError::NotFound("no readable plan holds this job".into()));
    }
    if let Some(error) = read_error {
        return Err(error);
    }
    plans.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.plan_id.cmp(&b.plan_id))
    });

    // Read only after a plan is admitted, so a failure cannot turn the `404` into
    // a `500`. The withheld tier gets the source without its eligibility fields.
    let (body, cancelled) = futures::try_join!(
        async {
            state
                .plan_store
                .get_job_source(job_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read the job source")))
        },
        // One listing rather than a marker read per plan, as `head_statuses` does.
        async {
            state.plan_store.list_cancel_markers().await.map_err(|e| {
                AppError::internal(e.context("failed to list the pending cancel markers"))
            })
        },
    )?;
    let body = body.map(|stored| ServedJobSource::from_stored(&stored, read.fleet));
    let cancelled: HashSet<PlanId> = cancelled.into_iter().collect();
    if body.is_none() {
        tracing::warn!("a linked job has no job source");
    } else if read.fleet == FleetDetail::Withheld {
        tracing::info!(
            job_id = %job_id,
            plan_ids = ?plans.iter().map(|plan| &plan.plan_id).collect::<Vec<_>>(),
            fleet_detail = read.fleet.label(),
            "plans by job: job eligibility withheld"
        );
    }
    Ok(JobPlans {
        fleet_detail: read.fleet,
        plans: plans
            .into_iter()
            .map(|plan| {
                let cancel_requested = cancelled.contains(&plan.plan_id);
                plan.view(cancel_requested)
            })
            .collect(),
        body: body.map(ServedJobSource::into_value),
    })
}

/// Read the plan document that `manifest` names, with a store failure as this
/// service's `500`.
pub(crate) async fn read_source(
    state: &AppState,
    manifest: &PlanManifest,
) -> Result<SourceRead, AppError> {
    pipette_pipeline::plan_document::read_source(state.plan_store.as_ref(), manifest)
        .await
        .map_err(AppError::internal)
}

// GET /api/v1/plans/{plan_id}/jobs
pub async fn list_plan_jobs(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
    ApiQuery(query): ApiQuery<PlanJobsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let window = job_window(&query);
    let body = if query.results.unwrap_or(false) {
        plan_jobs(&state, caller, &plan_id, window).await?
    } else {
        Named {
            body: jobs(&state, caller, &plan_id, window).await?,
            names: None,
        }
    };
    Ok(Json(body))
}

/// The plan page's jobs: [`with_results`]'s page under the plan scope alone, with
/// what the page offers and the device and vault names it shows.
async fn plan_jobs(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
    window: JobWindow,
) -> Result<Named<PlanJobs>, AppError> {
    let user = caller.user;
    let (manifest, read) = read_plan(state, caller, plan_id).await?;
    let (mut page, (cancel, may_edit_note), document) = futures::try_join!(
        with_results(state, caller, &manifest, window, &read),
        async {
            let campaign = PlanCampaign::read(state, user, &read.rights, &manifest).await?;
            Ok((
                cancel_grant(state, user, &read.rights, &manifest, campaign.as_ref()).await?,
                note_editable(user, &read.rights, &manifest, campaign.as_ref()),
            ))
        },
        document_state(state, &manifest, read.fleet),
    )?;
    // Lease holders and rows name no machine below the full tier.
    let devices = match read.fleet {
        FleetDetail::Full => {
            // Owned ids: a borrowed iterator breaks the handler's `Send` bound.
            let devices: Vec<ClientId> = page
                .jobs
                .iter()
                .flat_map(|row| {
                    let held = row
                        .job
                        .queue
                        .as_ref()
                        .and_then(|queue| queue.lease.as_ref());
                    let ran = match &row.result {
                        Some(JobResult::Scored { rows, .. }) => rows.as_slice(),
                        Some(JobResult::NotRun | JobResult::Unknown) | None => &[],
                    };
                    held.and_then(|lease| lease.client_id.clone())
                        .into_iter()
                        .chain(
                            ran.iter()
                                .filter_map(|row| ClientId::try_new(row.client_id.clone()?).ok())
                                .filter(|id| !is_system_client(id)),
                        )
                })
                .collect();
            Some(devices)
        }
        FleetDetail::Withheld => None,
    };
    let resources = planned_vault_ids(&page.jobs);
    with_planned_digests(&mut page.jobs);
    let digests = descriptor_digests(&page.jobs);
    let (devices, vault, descriptor_names) = futures::join!(
        async {
            match devices {
                Some(devices) => Some(device_names(state, devices.into_iter()).await),
                None => None,
            }
        },
        async {
            if resources.is_empty() {
                return Ok(BTreeMap::new());
            }
            let reach = VaultReach::resolve(state, caller).await?;
            Ok::<_, AppError>(vault_names(state, &reach, resources).await)
        },
        descriptor_names(state, caller, digests),
    );
    let vault = vault?;
    let names = match devices {
        Some(mut names) => {
            names.extend(vault);
            Some(names)
        }
        None => Some(vault).filter(|vault| !vault.is_empty()),
    };
    Ok(Named {
        body: PlanJobs {
            may_cancel: Some(read.rights.holds(cancel)),
            cancel_permission: Some(cancel),
            may_edit_note: Some(may_edit_note),
            document_state: Some(document),
            descriptor_names,
            ..page
        },
        names,
    })
}

/// Fill each job's `planned_runtime_sha256` and `planned_model_sha256`. A plan
/// repeats a few runtimes and models over many cells. This function hashes each
/// distinct runtime and model once. It hashes each model without its
/// `auth_token`, as the scorer stores it.
fn with_planned_digests(jobs: &mut [PlanJob]) {
    let mut runtimes: HashMap<&Runtime, Option<String>> = HashMap::new();
    let mut models: HashMap<&Model, Option<String>> = HashMap::new();
    jobs.iter_mut().for_each(|row| {
        let Some(cell) = row.job.planned.as_ref() else {
            return;
        };
        row.planned_runtime_sha256 = runtimes
            .entry(&cell.spec.runtime)
            .or_insert_with(|| typed_descriptor_sha256(&cell.spec.runtime).ok())
            .clone();
        row.planned_model_sha256 = models
            .entry(&cell.spec.model)
            .or_insert_with(|| typed_descriptor_sha256(&cell.spec.model.without_auth_token()).ok())
            .clone();
    });
}

/// Every runtime digest and model digest that the jobs show: the planned
/// ones, and the ones that their result rows report.
fn descriptor_digests(jobs: &[PlanJob]) -> BTreeSet<String> {
    jobs.iter()
        .flat_map(|row| {
            let ran = match &row.result {
                Some(JobResult::Scored { rows, .. }) => rows.as_slice(),
                Some(JobResult::NotRun | JobResult::Unknown) | None => &[],
            };
            row.planned_runtime_sha256
                .iter()
                .chain(&row.planned_model_sha256)
                .chain(ran.iter().flat_map(|row| {
                    row.runtime_descriptor_sha256
                        .iter()
                        .chain(&row.model_descriptor_sha256)
                }))
                .cloned()
        })
        .collect()
}

/// Every vault resource the plan cells of `jobs` name, in a model or a runtime.
fn planned_vault_ids(jobs: &[PlanJob]) -> HashSet<VaultResourceId> {
    let cells = || jobs.iter().filter_map(|row| row.job.planned.as_ref());
    // A plan repeats a few descriptors over many cells: each is walked once.
    let models: HashSet<&Model> = cells().map(|cell| &cell.spec.model).collect();
    let runtimes: HashSet<&Runtime> = cells().map(|cell| &cell.spec.runtime).collect();
    models
        .into_iter()
        .map(named_resources)
        .chain(runtimes.into_iter().map(named_resources))
        .flatten()
        .collect()
}

/// The document's state for [`PlanJobs`], without its text. There a withheld
/// reader's is not read: the manifest's digest says whether there is one.
async fn document_state(
    state: &AppState,
    manifest: &PlanManifest,
    fleet: FleetDetail,
) -> Result<DocumentState, AppError> {
    if !crate::api_v1::plans::discloses_document(fleet) {
        return Ok(match manifest.source_sha256 {
            Some(_) => DocumentState::Present,
            None => DocumentState::NoDocument,
        });
    }
    Ok(match read_source(state, manifest).await? {
        SourceRead::NoSource => DocumentState::NoDocument,
        SourceRead::Collected => DocumentState::Collected,
        SourceRead::Mismatch => {
            tracing::error!(
                plan_id = %manifest.plan_id,
                "plan jobs: the stored plan document does not match the manifest's source_sha256"
            );
            DocumentState::Mismatch
        }
        SourceRead::Present(_) => DocumentState::Present,
    })
}

/// A page of one plan's jobs, and where each one stands.
async fn jobs(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
    window: JobWindow,
) -> Result<PlanJobs, AppError> {
    let (manifest, read) = read_plan(state, caller, plan_id).await?;
    let cells = cells(state, &manifest).await?;
    let page = page_ids(&cells, window);
    // Neither read needs the other, so the queue lists while the runs read.
    let ((results, runs), queue) = futures::try_join!(
        async { Ok(read_results(state, &manifest, &page, window, read.fleet).await) },
        queue_listings(state, &manifest, &page),
    )?;
    jobs_of(
        state, &manifest, &cells, window, read.fleet, results, &runs, queue,
    )
    .await
}

/// The ids of `window`'s jobs, in the plan's order.
fn page_ids(cells: &[(JobId, Option<PlanCell>)], window: JobWindow) -> Vec<JobId> {
    let (offset, limit) = window.bounds(cells.len());
    crate::list::page_of(cells, offset, limit)
        .iter()
        .map(|(job_id, _)| job_id.clone())
        .collect()
}

/// The jobs one read answers: a page of the plan's, or every one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobWindow {
    Page { offset: usize, limit: usize },
    All,
}

impl JobWindow {
    /// `(offset, limit)` over a plan of `total` jobs.
    fn bounds(self, total: usize) -> (usize, usize) {
        match self {
            Self::Page { offset, limit } => (offset, limit),
            Self::All => (0, total),
        }
    }

    /// How long a page of this window waits on its results reads, together.
    fn results_wait(self, db: &ResultsDb) -> std::time::Duration {
        match self {
            Self::Page { .. } => db.job_results_timeout(),
            Self::All => db.timeout(),
        }
    }
}

/// One window for a page of jobs and for its results, so the two cannot drift.
fn job_window(query: &PlanJobsQuery) -> JobWindow {
    if query.all.unwrap_or(false) {
        return JobWindow::All;
    }
    // A `limit` over the ceiling is clamped, not refused; `0` asks for the count alone.
    JobWindow::Page {
        offset: query.offset.unwrap_or(0),
        limit: query.limit.unwrap_or(JOBS_PAGE_DEFAULT).min(JOBS_PAGE_MAX),
    }
}

/// Each of `manifest`'s jobs and its cell, in the order its document lists
/// them where it still reads: kept from an earlier read, else read off the
/// jobs' stored sources.
async fn cells(state: &AppState, manifest: &PlanManifest) -> Result<JobCells, AppError> {
    if let Some(kept) = state
        .cell_cache
        .get(&manifest.plan_id, manifest.job_ids.len())
    {
        return Ok(kept);
    }
    let (mut requests, source) = futures::try_join!(
        stored_requests(state, manifest),
        read_source(state, manifest),
    )?;
    let order = document_order(state, manifest, source).await?;
    // In job order wherever the document sets none.
    let mut cells: Vec<(JobId, Option<PlanCell>)> = manifest
        .job_ids
        .iter()
        .map(|job_id| (job_id.clone(), requests.remove(job_id)))
        .collect();
    if let Some(order) = order {
        in_document_order(&mut cells, &order);
    }
    let cells: JobCells = cells.into();
    // Ingestion writes the manifest before the jobs' sources.
    if manifest.status != PlanStatus::Creating {
        state.cell_cache.keep(&manifest.plan_id, &cells);
    }
    Ok(cells)
}

/// The cells `manifest`'s document lists, in its order; `None` for a plan
/// whose document is gone or no longer reads, whose jobs keep their order.
async fn document_order(
    state: &AppState,
    manifest: &PlanManifest,
    source: SourceRead,
) -> Result<Option<Vec<PlanCell>>, AppError> {
    let SourceRead::Present(source) = source else {
        return Ok(None);
    };
    let ids = pipette_planner::benchmark_ids(source.text()).unwrap_or_default();
    let types = filed_types(&manifest.benchmark_types, &ids);
    let cells = spawn_document_cells(source, state.config.plan_max_jobs.get(), types)
        .await
        .map_err(|e| AppError::internal(anyhow::Error::new(e).context("plan cells task failed")))?;
    if cells.is_none() {
        tracing::warn!(
            plan_id = %manifest.plan_id,
            "plan jobs: the stored plan document no longer expands; the jobs keep their order"
        );
    }
    Ok(cells)
}

/// Every result row of a plan, with the plan's manifest. The rows follow the
/// scope and fleet tier of `GET /api/v1/plans/{plan_id}/jobs`, and they carry
/// the parameter counts that the route fills.
pub(crate) async fn all_results(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
) -> Result<(PlanManifest, Vec<ResultRow>), AppError> {
    let (manifest, read) = read_plan(state, caller, plan_id).await?;
    let mut rows = results_db(state)?
        .all_plan_results(plan_id)
        .await
        .map_err(results_error)?;
    fill_model_params(state, rows.iter_mut()).await;
    if read.fleet == FleetDetail::Withheld {
        rows.iter_mut().for_each(ResultRow::withhold_fleet);
    }
    Ok((manifest, rows))
}

// GET /api/v1/plans/{plan_id}/results.csv
//
// Errors answer in the JSON envelope.
pub async fn get_plan_results_csv(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
) -> Result<Response, AppError> {
    let caller = active.caller();
    results_file(&state, caller, &plan_id).await
}

/// Every result row of a plan as one CSV file to download ([`all_results`]'s
/// gates, then [`results_csv`]). The file takes its name from [`results_stem`].
pub(crate) async fn results_file(
    state: &AppState,
    caller: Caller<'_>,
    plan_id: &PlanId,
) -> Result<Response, AppError> {
    let user = caller.user;
    let (manifest, rows) = all_results(state, caller, plan_id).await?;
    let digests = rows
        .iter()
        .flat_map(|row| {
            row.runtime_descriptor_sha256
                .iter()
                .chain(&row.model_descriptor_sha256)
                .cloned()
        })
        .collect();
    let names = descriptor_names(state, caller, digests)
        .await
        .unwrap_or_default();
    let body = results_csv(&rows, &names).map_err(|e| {
        AppError::internal(anyhow::Error::from(e).context("failed to write the results as csv"))
    })?;
    let stem = results_stem(state, caller, &manifest).await;
    tracing::info!(
        user_id = %user.user_id,
        rows = rows.len(),
        "plan results downloaded as csv"
    );
    Ok((
        [
            (CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (CONTENT_DISPOSITION, results_disposition(&stem)),
        ],
        body,
    )
        .into_response())
}

/// The name of a plan's results file, without `.csv`.
///
/// A plan in a campaign takes `{campaign}-plan-{n}`. `campaign` is the
/// campaign's name if the caller may read the campaign, cut to
/// [`NAME_MAX_BYTES`], and the campaign's id if not. `n` is the plan's [`campaign_chain::position`]. If the position is
/// not known, the name is `{campaign}` alone. A plan outside every campaign
/// takes `{plan_id}-results`.
async fn results_stem(state: &AppState, caller: Caller<'_>, manifest: &PlanManifest) -> String {
    let Some(campaign_id) = &manifest.campaign_id else {
        return format!("{}-results", manifest.plan_id);
    };
    let campaign = match readable_campaign(state, caller, campaign_id).await {
        Ok(Some(campaign)) => clipped(&campaign.name),
        Ok(None) => campaign_id.to_string(),
        Err(e) => {
            // The name is a convenience, so the file goes out under the id.
            tracing::warn!(
                campaign_id = %campaign_id,
                error = %e,
                "results file named by campaign id: campaign read failed"
            );
            campaign_id.to_string()
        }
    };
    match campaign_chain::position(state.plan_store.as_ref(), manifest).await {
        Ok(Some(position)) => format!("{campaign}-plan-{position}"),
        Ok(None) => {
            tracing::warn!(
                campaign_id = %campaign_id,
                "results file named without a plan number: the campaign history is broken"
            );
            campaign
        }
        Err(e) => {
            tracing::warn!(
                campaign_id = %campaign_id,
                error = %e,
                "results file named without a plan number: campaign history read failed"
            );
            campaign
        }
    }
}

/// The most UTF-8 bytes of a campaign's name that a results file name holds.
/// With `-plan-{n}.csv` added, the name stays below the 255-byte limit of
/// common file systems.
const NAME_MAX_BYTES: usize = 200;

/// The longest start of `name` that fits in [`NAME_MAX_BYTES`]. The cut falls
/// between two characters.
fn clipped(name: &str) -> String {
    name.chars()
        .scan(0, |len, c| {
            *len += c.len_utf8();
            (*len <= NAME_MAX_BYTES).then_some(c)
        })
        .collect()
}

/// The campaign that `campaign_id` names. `None` if the campaign does not exist
/// or the caller may not read it.
async fn readable_campaign(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<Option<Campaign>, AppError> {
    let Some(reach) = CampaignReach::over_records_if_granted(state, caller).await? else {
        return Ok(None);
    };
    let campaign = state
        .campaign_store
        .get_campaign(campaign_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the campaign")))?;
    Ok(campaign.filter(|campaign| reach.admits(campaign)))
}

/// The bytes that a `filename*` percent-encodes: all bytes outside the
/// `attr-char` set of RFC 5987.
const ATTR_CHAR_ESCAPES: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// The `Content-Disposition` of a results file named `{stem}.csv`.
///
/// The function replaces some characters in `stem` with `_`, so the name is a
/// safe file name on every common file system. These are the control
/// characters, the invisible format and separator characters, and each of
/// `/ \ : * ? " < > |`. `filename*` carries the name as UTF-8. `filename`
/// carries the name for clients that read only ASCII, with `_` in place of each
/// non-ASCII character.
fn results_disposition(stem: &str) -> String {
    let safe: String = stem
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                || matches!(
                    c,
                    '\u{200B}'..='\u{200F}'
                        | '\u{2028}'..='\u{202E}'
                        | '\u{2060}'..='\u{206F}'
                        | '\u{FEFF}'
                )
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let ascii: String = safe
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    let encoded = utf8_percent_encode(&safe, ATTR_CHAR_ESCAPES);
    format!("attachment; filename=\"{ascii}.csv\"; filename*=UTF-8''{encoded}.csv")
}

fn results_db(state: &AppState) -> Result<&ResultsDb, AppError> {
    state.results_db.as_ref().ok_or_else(|| {
        AppError::NotFound("plan results are not served: no [duckdb] is configured".into())
    })
}

fn results_error(e: ResultsError) -> AppError {
    match e {
        ResultsError::Window { .. } => AppError::BadRequest(e.to_string()),
        ResultsError::Url(_)
        | ResultsError::Unreachable(_)
        | ResultsError::Status { .. }
        | ResultsError::Body(_)
        | ResultsError::Row(_)
        | ResultsError::Counts
        | ResultsError::DeviceTime => AppError::BadGateway(format!("{:#}", anyhow::Error::from(e))),
    }
}

/// A page of `manifest`'s jobs with their results, each reason whole, and the
/// re-runs, recalculations and parent rows the plan's page offers on them.
async fn with_results(
    state: &AppState,
    caller: Caller<'_>,
    manifest: &PlanManifest,
    window: JobWindow,
    read: &PlanRead,
) -> Result<PlanJobs, AppError> {
    let fleet = read.fleet;
    let campaign_id = manifest.campaign_id.as_ref();
    let ((cells, page, results, queue), rerun_campaign_id) = futures::try_join!(
        async {
            // An all read needs queue listings only when the complete plan
            // has expired jobs. Its manifest names every job, so the listing
            // can run beside a cold cache fill before the cells are ready.
            let (kept, early_queue) =
                futures::try_join!(kept_results(state, manifest, window), async {
                    if window == JobWindow::All && manifest.status == PlanStatus::Complete {
                        queue_listings(state, manifest, &manifest.job_ids)
                            .await
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                })?;
            let cells = match &kept {
                Some(kept) => kept.cells.clone(),
                None => cells(state, manifest).await?,
            };
            let page = page_ids(&cells, window);
            let (results, queue) = futures::try_join!(
                async {
                    Ok(match kept {
                        Some(kept) => kept,
                        None => Arc::new(
                            read_plan_results(state, manifest, cells.clone(), &page, window).await,
                        ),
                    })
                },
                async {
                    match early_queue {
                        Some(queue) => Ok(queue),
                        None => queue_listings(state, manifest, &page).await,
                    }
                },
            )?;
            Ok::<_, AppError>((cells, page, results, queue))
        },
        rerun_campaign(state, caller, &read.rights, campaign_id, &manifest.plan_id),
    )?;
    // Every read is at the full tier, and a kept plan's runs are the whole
    // plan's: cut them to the page and to the caller's tier.
    let PlanJobResults {
        read: results,
        mut runs,
        history: History {
            mut earlier,
            parents,
            ..
        },
        ..
    } = Arc::unwrap_or_clone(results);
    let on_page: HashSet<&JobId> = page.iter().collect();
    runs.retain(|job_id, _| on_page.contains(job_id));
    if fleet == FleetDetail::Withheld {
        runs.values_mut()
            .chain(earlier.values_mut())
            .flatten()
            .for_each(ResultRow::withhold_fleet);
    }
    let jobs = jobs_of(
        state, manifest, &cells, window, fleet, results, &runs, queue,
    )
    .await?;
    let mut page = with_job_results(jobs, runs, results);
    // The catalog is asked only where the page offers a recalculation, as the console's.
    let evals = match rerun_campaign_id {
        Some(_) => finished_evals(state, &page.jobs).await,
        None => BTreeSet::new(),
    };
    page.jobs.iter_mut().for_each(|row| {
        let plan = &mut row.job.plan;
        if let Some(parent) = &mut plan.rerun_of {
            parent.plan_id = parents.plan_of(&parent.job_id);
            plan.rerun_history = parents.rerun_history(&parent.job_id);
        }
        if let Some(JobResult::Scored {
            rescored_from,
            earlier_runs,
            recalculable,
            ..
        }) = &mut row.result
        {
            *recalculable = evals.contains(&row.job.job_id);
            if let Some(parent) = rescored_from {
                parent.plan_id = parents.plan_of(&parent.job_id);
                *earlier_runs = parents.rescored_runs(&parent.job_id, &earlier);
            } else if let Some(parent) = &plan.rerun_of {
                *earlier_runs = parents.rerun_runs(&parent.job_id, &earlier);
            }
        }
    });
    Ok(PlanJobs {
        rerun_campaign_id,
        ..page
    })
}

/// The results of a `complete` plan that [`AppState::plan_results_cache`]
/// keeps. An `all` read fills the cache. A page read uses an entry that is
/// already there, and gets `None` if there is no entry.
async fn kept_results(
    state: &AppState,
    manifest: &PlanManifest,
    window: JobWindow,
) -> Result<Option<Arc<PlanJobResults>>, AppError> {
    if manifest.status != PlanStatus::Complete || state.results_db.is_none() {
        return Ok(None);
    }
    let plan_id = &manifest.plan_id;
    match window {
        JobWindow::Page { .. } => Ok(state.plan_results_cache.get(plan_id)),
        JobWindow::All => {
            let fill = || async {
                let cells = cells(state, manifest).await?;
                let page = page_ids(&cells, window);
                Ok::<_, AppError>(read_plan_results(state, manifest, cells, &page, window).await)
            };
            state
                .plan_results_cache
                .get_or_fill(plan_id, fill)
                .await
                .map(Some)
        }
    }
}

/// The runs of `page` at the full tier, with their parameter counts, and the
/// history above the page's re-runs and recalculations. The function logs
/// each failed read and records the failure in the answer.
async fn read_plan_results(
    state: &AppState,
    manifest: &PlanManifest,
    cells: JobCells,
    page: &[JobId],
    window: JobWindow,
) -> PlanJobResults {
    let campaign_id = manifest.campaign_id.as_ref();
    // One deadline for every results read of the page, so the earlier runs
    // read within the page's wait rather than after it.
    let deadline = state
        .results_db
        .as_ref()
        .map(|db| tokio::time::Instant::now() + window.results_wait(db));
    // The plan names the job each re-run ran again, so a re-run's history reads
    // beside the runs. A recalculation's run names its parent, so its history
    // reads as soon as the runs are in.
    let reran: HashSet<JobId> = page
        .iter()
        .filter_map(|job_id| manifest.rerun_of.get(job_id))
        .cloned()
        .collect();
    // The runs come first: they settle the jobs that have one.
    let ((read, mut runs, rescores), mut history) = futures::join!(
        async {
            let (read, runs) = read_results(state, manifest, page, window, FleetDetail::Full).await;
            let rescored = runs.values().filter_map(|run| rescored_of(run)).collect();
            let rescores = runs_before(state, campaign_id, rescored, window, deadline).await;
            (read, runs, rescores)
        },
        rerun_reads(state, campaign_id, reran, window, deadline),
    );
    // Each history read its own parents whole, so a job both reached is the same in each.
    history.extend(rescores);
    fill_model_params(state, runs.values_mut().flatten()).await;
    PlanJobResults {
        cells,
        read,
        runs,
        history,
    }
}

/// `campaign_id`, the campaign of `plan_id`, when `user` may re-run its jobs
/// there ([`crate::api_v1::campaigns::reads::may_rerun`]).
pub(crate) async fn rerun_campaign(
    state: &AppState,
    caller: Caller<'_>,
    rights: &UserRights,
    campaign_id: Option<&CampaignId>,
    plan_id: &PlanId,
) -> Result<Option<CampaignId>, AppError> {
    // `revisable_head` refuses anyone else, so a reader who can never revise
    // costs no campaign reads and logs no refusal.
    let Some(campaign_id) = campaign_id.filter(|_| {
        rights.holds(UserPermission::SubmitPlans)
            && crate::api_v1::plans::discloses_document(rights.fleet_detail())
    }) else {
        return Ok(None);
    };
    let may =
        crate::api_v1::campaigns::reads::may_rerun(state, caller, campaign_id, plan_id).await?;
    Ok(may.then(|| campaign_id.clone()))
}

/// The deepest re-run or recalculation history a page follows. Chains are
/// short in practice; the cap bounds the reads a page makes.
const HISTORY_LIMIT: usize = 16;

/// The history above a page's re-runs and recalculations: the rows of each
/// earlier run, by job, and the plans that hold them.
#[derive(Clone)]
pub(super) struct History {
    earlier: HashMap<JobId, Vec<ResultRow>>,
    parents: Parents,
    /// Whether each read of the history came back. A history cut at
    /// [`HISTORY_LIMIT`] is whole.
    pub(super) whole: bool,
}

impl History {
    fn new(earlier: HashMap<JobId, Vec<ResultRow>>, parents: Parents, whole: bool) -> Self {
        Self {
            earlier,
            parents,
            whole,
        }
    }

    /// `other`'s runs and plans too. Both read the same plans, so a job that
    /// both reached is the same in each.
    fn extend(&mut self, other: Self) {
        self.earlier.extend(other.earlier);
        self.parents.extend(other.parents);
        self.whole &= other.whole;
    }

    /// The number of result rows in the history.
    pub(super) fn rows(&self) -> usize {
        self.earlier.values().map(Vec::len).sum()
    }
}

/// No history, which is whole.
impl Default for History {
    fn default() -> Self {
        Self::new(HashMap::new(), Parents::default(), true)
    }
}

/// The plan each of a page's parent jobs is found in, and the job each re-run
/// among them ran again.
#[derive(Clone, Default)]
struct Parents {
    /// Keyed by each parent and each earlier run.
    plans: HashMap<JobId, PlanId>,
    /// The `rerun_of` of each parent and earlier run that a re-run minted.
    ran_again: HashMap<JobId, JobId>,
}

impl Parents {
    /// The runs before `rerun_of`, newest first: the job it ran again, the job
    /// that one ran again, and so on.
    fn rerun_history(&self, rerun_of: &JobId) -> Vec<ParentJob> {
        std::iter::successors(self.ran_again.get(rerun_of), |job_id| {
            self.ran_again.get(*job_id)
        })
        .take(HISTORY_LIMIT)
        .map(|job_id| ParentJob {
            job_id: job_id.clone(),
            plan_id: self.plan_of(job_id),
        })
        .collect()
    }

    fn plan_of(&self, job_id: &JobId) -> Option<PlanId> {
        self.plans.get(job_id).cloned()
    }

    /// `other`'s jobs too. Both walks read the same plans, so a job either
    /// reached maps to the same plan and the same earlier job in each.
    fn extend(&mut self, other: Self) {
        self.plans.extend(other.plans);
        self.ran_again.extend(other.ran_again);
    }

    /// `rescored_from`'s run and each run it scored again, newest first.
    fn rescored_runs(
        &self,
        rescored_from: &JobId,
        rows: &HashMap<JobId, Vec<ResultRow>>,
    ) -> Vec<ParentRun> {
        std::iter::successors(rows.get_key_value(rescored_from), |(_, run)| {
            rows.get_key_value(&rescored_of(run)?)
        })
        .take(HISTORY_LIMIT)
        .map(|run| self.parent_run(run))
        .collect()
    }

    /// `rerun_of`'s run and each run before it that has rows, newest first.
    fn rerun_runs(
        &self,
        rerun_of: &JobId,
        rows: &HashMap<JobId, Vec<ResultRow>>,
    ) -> Vec<ParentRun> {
        std::iter::successors(Some(rerun_of), |job_id| self.ran_again.get(*job_id))
            .take(HISTORY_LIMIT + 1)
            .filter_map(|job_id| rows.get_key_value(job_id))
            .map(|run| self.parent_run(run))
            .collect()
    }

    fn parent_run(&self, (job_id, rows): (&JobId, &Vec<ResultRow>)) -> ParentRun {
        ParentRun {
            job: ParentJob {
                job_id: job_id.clone(),
                plan_id: self.plan_of(job_id),
            },
            rows: rows.iter().cloned().map(Into::into).collect(),
        }
    }
}

/// For each job in `frontier`, find the newest plan in `campaign_id` that
/// holds it. Follow one hop per round. Read each plan once. The flag is
/// `false` if a read failed.
async fn parent_plans(
    state: &AppState,
    campaign_id: Option<&CampaignId>,
    mut frontier: HashSet<JobId>,
) -> (Parents, bool) {
    // A recalculation or a re-run names jobs of the plan its campaign runs, so
    // its parent is in that campaign and a plan outside one has none.
    let Some(campaign_id) = campaign_id else {
        return (Parents::default(), true);
    };
    let mut seen = frontier.clone();
    // `None` for a plan that did not read or is outside the campaign.
    let mut manifests: HashMap<PlanId, Option<PlanManifest>> = HashMap::new();
    let mut parents = Parents::default();
    let mut whole = true;
    for _ in 0..=HISTORY_LIMIT {
        if frontier.is_empty() {
            break;
        }
        let asked: Vec<JobId> = frontier.drain().collect();
        let links: Vec<(JobId, Vec<PlanId>)> = state
            .plan_store
            .list_job_plan_links_of(&asked)
            .await
            .inspect_err(|e| {
                whole = false;
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "the rows' parent jobs did not list their plans; the rows name them unlinked"
                );
            })
            .unwrap_or_default();
        let unread: Vec<PlanId> = links
            .iter()
            .flat_map(|(_, plan_ids)| plan_ids)
            .filter(|plan_id| !manifests.contains_key(*plan_id))
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let read = state
            .plan_store
            .get_plans(&unread)
            .await
            .inspect_err(|e| {
                whole = false;
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "a row's parent jobs' plans did not read; the rows name them unlinked"
                );
            })
            .unwrap_or_default();
        read.unreadable.iter().for_each(|entry| {
            whole = false;
            tracing::warn!(
                %entry,
                "a row's parent job's plan did not read; the row names it unlinked"
            );
        });
        // Settle every id first, so a plan that did not read is not read again;
        // the reads below overwrite the ones that came back.
        manifests.extend(unread.into_iter().map(|plan_id| (plan_id, None)));
        manifests.extend(
            read.items
                .into_iter()
                .filter(|manifest| manifest.campaign_id.as_ref() == Some(campaign_id))
                .map(|manifest| (manifest.plan_id.clone(), Some(manifest))),
        );
        for (job_id, plan_ids) in links {
            let Some(manifest) = plan_ids
                .iter()
                .filter_map(|plan_id| manifests.get(plan_id)?.as_ref())
                .filter(|m| m.job_ids.contains(&job_id))
                .max_by(|a, b| {
                    a.created_at
                        .cmp(&b.created_at)
                        .then_with(|| a.plan_id.cmp(&b.plan_id))
                })
            else {
                continue;
            };
            parents
                .plans
                .insert(job_id.clone(), manifest.plan_id.clone());
            if let Some(earlier) = manifest.rerun_of.get(&job_id) {
                if seen.insert(earlier.clone()) {
                    frontier.insert(earlier.clone());
                }
                parents.ran_again.insert(job_id, earlier.clone());
            }
        }
    }
    (parents, whole)
}

/// The rows of `jobs`' runs and of each run they score again, and the plans
/// that hold each: the history above a page's re-runs or recalculations.
async fn runs_before(
    state: &AppState,
    campaign_id: Option<&CampaignId>,
    jobs: HashSet<JobId>,
    window: JobWindow,
    deadline: Option<tokio::time::Instant>,
) -> History {
    let (earlier, rows_whole) = earlier_rows(state, jobs.clone(), window, deadline).await;
    let asked = jobs.into_iter().chain(earlier.keys().cloned()).collect();
    let (parents, plans_whole) = parent_plans(state, campaign_id, asked).await;
    History::new(earlier, parents, rows_whole && plans_whole)
}

/// What a re-run's history shows: [`runs_before`] the jobs `reran` names, then
/// the rows of the older runs, which the plans name and so read after them.
async fn rerun_reads(
    state: &AppState,
    campaign_id: Option<&CampaignId>,
    reran: HashSet<JobId>,
    window: JobWindow,
    deadline: Option<tokio::time::Instant>,
) -> History {
    let mut history = runs_before(state, campaign_id, reran.clone(), window, deadline).await;
    let older: HashSet<JobId> = reran
        .iter()
        .flat_map(|job_id| history.parents.rerun_history(job_id))
        .map(|parent| parent.job_id)
        .filter(|job_id| !history.earlier.contains_key(job_id))
        .collect();
    let (older_rows, rows_whole) = earlier_rows(state, older.clone(), window, deadline).await;
    // The runs an older run scores again, which a recalculation on the page may
    // show too: their plans are named here, and usually there are none to ask.
    let rescored = older_rows
        .keys()
        .filter(|job_id| !older.contains(*job_id))
        .cloned()
        .collect();
    let (parents, plans_whole) = parent_plans(state, campaign_id, rescored).await;
    history.extend(History::new(older_rows, parents, rows_whole && plans_whole));
    history
}

/// Return full-tier rows for the runs in `frontier` and each run they score
/// again, grouped by job. Use one query per hop. Complete each query by
/// `deadline`. A failed read ends the chain, and the flag is `false`.
async fn earlier_rows(
    state: &AppState,
    mut frontier: HashSet<JobId>,
    window: JobWindow,
    deadline: Option<tokio::time::Instant>,
) -> (HashMap<JobId, Vec<ResultRow>>, bool) {
    let mut found = HashMap::new();
    let (Some(db), Some(deadline)) = (state.results_db.as_ref(), deadline) else {
        return (found, true);
    };
    let mut whole = true;
    let mut seen = frontier.clone();
    for _ in 0..HISTORY_LIMIT {
        if frontier.is_empty() {
            break;
        }
        let asked: Vec<JobId> = frontier.drain().collect();
        let read =
            tokio::time::timeout_at(deadline, rows_of(db, &asked, window, FleetDetail::Full))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|rows| rows.map_err(anyhow::Error::from));
        let read = match read {
            Ok(read) => read,
            Err(e) => {
                whole = false;
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "the jobs' earlier runs did not read; the jobs answer without them"
                );
                break;
            }
        };
        frontier.extend(
            read.values()
                .filter_map(|run| rescored_of(run))
                .filter(|job_id| seen.insert(job_id.clone())),
        );
        found.extend(read);
    }
    fill_model_params(state, found.values_mut().flatten()).await;
    (found, whole)
}

/// The finished jobs of `jobs` whose run is an eval, which a recalculation
/// takes. A catalog that does not read offers none; the route still decides.
async fn finished_evals(state: &AppState, jobs: &[PlanJob]) -> BTreeSet<JobId> {
    let public = match state.catalog_cache.get().await {
        Ok(public) => public,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "the benchmark catalog did not read; the page offers no recalculation"
            );
            return BTreeSet::new();
        }
    };
    let finished: Vec<(&JobId, BenchmarkId)> = jobs
        .iter()
        .filter(|row| row.job.state == JobState::Finished)
        .filter_map(|row| {
            let Some(JobResult::Scored { rows, .. }) = &row.result else {
                return None;
            };
            let benchmark_id = BenchmarkId::try_new(rows.first()?.benchmark_id.as_str()).ok()?;
            Some((&row.job.job_id, benchmark_id))
        })
        .collect();
    // Owned ids: a stream of borrowed ones breaks the handler's `Send` bound.
    let benchmarks: BTreeSet<BenchmarkId> = finished.iter().map(|(_, id)| id.clone()).collect();
    let evals: HashSet<BenchmarkId> = stream::iter(benchmarks.into_iter().map(|benchmark_id| {
        let public = &public;
        async move {
            let benchmark =
                org_catalog::server_benchmark(state.catalog_store.as_ref(), public, &benchmark_id)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!(
                            benchmark_id = %benchmark_id,
                            error = %format!("{e:#}"),
                            "a job's benchmark did not read; its rows offer no recalculation"
                        );
                    })
                    .ok()
                    .flatten();
            benchmark
                .is_some_and(|b| b.benchmark_type().is_scored_via_server())
                .then_some(benchmark_id)
        }
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .filter_map(std::future::ready)
    .collect()
    .await;
    finished
        .iter()
        .filter(|(_, benchmark_id)| evals.contains(benchmark_id))
        .map(|(job_id, _)| (*job_id).clone())
        .collect()
}

/// `page`'s result rows by job, cut to `fleet`, and how they were read. The
/// whole plan's read waits `[duckdb] timeout_secs`; a page's, at most
/// `JOB_RESULTS_TIMEOUT`. A failure is logged: the jobs still answer.
async fn read_results(
    state: &AppState,
    manifest: &PlanManifest,
    page: &[JobId],
    window: JobWindow,
    fleet: FleetDetail,
) -> (ResultsRead, HashMap<JobId, Vec<ResultRow>>) {
    let Some(db) = state.results_db.as_ref() else {
        return (ResultsRead::Off, HashMap::new());
    };
    match rows_of(db, page, window, fleet).await {
        Ok(rows) => (ResultsRead::Ok, rows),
        Err(e) => {
            let read = if e.is_timeout() {
                ResultsRead::TimedOut
            } else {
                ResultsRead::Failed
            };
            tracing::warn!(
                plan_id = %manifest.plan_id,
                error = %format!("{:#}", anyhow::Error::from(e)),
                "could not read the jobs' results from pipette-duckdb; the jobs answer without them"
            );
            (read, HashMap::new())
        }
    }
}

/// `jobs`' result rows by job, cut to `fleet`, read as `window` reads them.
async fn rows_of(
    db: &ResultsDb,
    jobs: &[JobId],
    window: JobWindow,
    fleet: FleetDetail,
) -> Result<HashMap<JobId, Vec<ResultRow>>, ResultsError> {
    let mut rows = match window {
        JobWindow::All => db.all_job_results(jobs).await,
        JobWindow::Page { .. } => db.job_results(jobs).await,
    }?;
    if fleet == FleetDetail::Withheld {
        rows.iter_mut().for_each(ResultRow::withhold_fleet);
    }
    Ok(rows_by_job(rows))
}

/// The queue's `avail/` keys and leases, for a page of `page`'s jobs: two
/// listings for the whole page, as each `TodoStore` call returns its whole
/// tree. A complete plan holds only lapsed jobs there, so only those need them.
async fn queue_listings(
    state: &AppState,
    manifest: &PlanManifest,
    page: &[JobId],
) -> Result<(Vec<AvailKey>, Vec<String>), AppError> {
    let queued = manifest.status != PlanStatus::Complete
        || page
            .iter()
            .any(|job_id| manifest.expired_jobs.contains_key(job_id));
    if !queued {
        return Ok((Vec::new(), Vec::new()));
    }
    futures::try_join!(
        async {
            collect_all_avail(state.todo_store.as_ref())
                .await
                .map(|listing| listing.keys)
                .map_err(|e| {
                    AppError::internal(e.context("failed to list the available-job queue"))
                })
        },
        async {
            state
                .todo_store
                .list_leased()
                .await
                .map_err(|e| AppError::internal(e.context("failed to list the leases")))
        },
    )
}

/// A window of `manifest`'s jobs: the half of [`jobs`] after the plan is read,
/// every key `results=true` adds absent. A job out of both queues is settled by
/// its rows, then its recorded outcome.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds; the caller reads the \
              listings beside the runs"
)]
async fn jobs_of(
    state: &AppState,
    manifest: &PlanManifest,
    cells: &[(JobId, Option<PlanCell>)],
    window: JobWindow,
    fleet: FleetDetail,
    results: ResultsRead,
    runs: &HashMap<JobId, Vec<ResultRow>>,
    (avail_keys, leased_keys): (Vec<AvailKey>, Vec<String>),
) -> Result<PlanJobs, AppError> {
    let plan_id = &manifest.plan_id;

    let (offset, limit) = window.bounds(cells.len());
    let page = crate::list::page_of(cells, offset, limit);
    let members: HashSet<&JobId> = page.iter().map(|(job_id, _)| job_id).collect();
    let now = Utc::now();

    let avail: HashMap<&JobId, &AvailKey> = avail_keys
        .iter()
        .filter(|key| members.contains(&key.job_id))
        .map(|key| (&key.job_id, key))
        .collect();

    // A key that does not parse names no job and is skipped; other passes warn.
    // Where the recycle race shows two leases, the live one wins.
    let mut leases: HashMap<JobId, Lease> = HashMap::new();
    for key in leased_keys {
        let Ok(lease) = parse_leased_key(&key) else {
            continue;
        };
        if !members.contains(&lease.job_id) {
            continue;
        }
        match leases.get(&lease.job_id) {
            Some(held) if held.lease_expiry > now || lease.lease_expiry <= now => {}
            _ => {
                leases.insert(lease.job_id.clone(), lease);
            }
        }
    }

    // This fan-out is the page's whole cost. Only a server with no `[duckdb]` reads a submission
    // record, a `find_job` that downloads it whole. `buffered`, not
    // `buffer_unordered`: the plan's order is what makes paging stable.
    let jobs: Vec<PlanJob> = stream::iter(page.iter().cloned().map(|(job_id, cell)| {
        // Everything read from the listings is resolved before the future is
        // built, so what is left inside it is the one store call.
        let entry = avail.get(&job_id).copied();
        let held = leases.get(&job_id);
        let queue = entry
            .map(|key| key.expires_at)
            .or_else(|| held.map(|lease| lease.expires_at))
            .map(|expires_at| JobQueue {
                expires_at: expires_at.instant(),
                lease: held.map(|lease| pipette_plan_types::Lease {
                    expires_at: lease.lease_expiry,
                    // It names a fleet member, so it is withheld below
                    // `read_clients`.
                    client_id: match fleet {
                        FleetDetail::Full => Some(lease.client_id.clone()),
                        FleetDetail::Withheld => None,
                    },
                }),
            });
        // The lease key's own coordinates, which are what address the leased
        // object.
        let held = held.map(|lease| {
            (
                lease.expires_at,
                lease.client_id.clone(),
                lease.lease_expiry,
            )
        });
        let lapsed = manifest.expired_jobs.get(&job_id).cloned().map(Into::into);
        let rerun_of = manifest.rerun_of.get(&job_id).cloned();
        let run = runs.get(&job_id);
        let settled = manifest.settled_jobs.get(&job_id).copied();

        async move {
            // The request of a job filed before job sources were stored.
            let mut queued = None;

            // `count_jobs`' order, so a page and `progress_snapshot` never
            // disagree. A lapse comes first: it is the plan's outcome even while
            // the job runs on for another member.
            let lifecycle = if lapsed.is_some() {
                JobState::Failed
            } else if let Some(key) = entry {
                if cell.is_none() {
                    // Addressed directly, because the `avail/` listing
                    // already holds this job's `expires_at`.
                    let body = state
                        .todo_store
                        .get_avail(&job_id, key.expires_at)
                        .await
                        .map_err(|e| {
                            AppError::internal(e.context("failed to read the queued job"))
                        })?;
                    queued = body
                        .as_ref()
                        .and_then(|body| body_request(&job_id, body, manifest));
                }
                JobState::Available
            } else if let Some((expires_at, client_id, lease_expiry)) = &held {
                if cell.is_none() {
                    let body = state
                        .todo_store
                        .get_leased(&job_id, *expires_at, client_id, *lease_expiry)
                        .await
                        .map_err(|e| {
                            AppError::internal(e.context("failed to read the leased job"))
                        })?;
                    queued = body
                        .as_ref()
                        .and_then(|body| body_request(&job_id, body, manifest));
                }
                JobState::Running
            } else if let Some(rows) = run {
                run_state(rows)
            } else if let Some(outcome) = settled {
                // Answered, its run not yet scored and republished: without
                // this a complete plan would call it `cancelled` until then.
                settled_state(outcome)
            } else if results != ResultsRead::Off {
                unsettled_state(results, manifest.status)
            } else {
                let record = state
                    .submission_store
                    .find_job(&job_id)
                    .await
                    .map_err(|e| AppError::internal(e.context("failed to find the job record")))?;
                match record {
                    // The tag is read off the body, not the record deserialized,
                    // so one unparseable body cannot wedge the page. An absent
                    // tag reads as success, as `parse_stored_submission` does.
                    Some(record)
                        if record.body.get("message_type").and_then(Value::as_str)
                            == Some("failure") =>
                    {
                        JobState::Failed
                    }
                    Some(_) => JobState::Finished,
                    None => JobState::Cancelled,
                }
            };
            let job = Job {
                planned: cell.or(queued).as_ref().map(Into::into),
                job_id,
                state: lifecycle,
                queue,
                plan: JobInPlan {
                    lapsed,
                    rerun_of: rerun_of.map(|job_id| ParentJob {
                        job_id,
                        plan_id: None,
                    }),
                    rerun_history: Vec::new(),
                },
            };
            Ok::<_, AppError>(PlanJob {
                job,
                result: None,
                planned_runtime_sha256: None,
                planned_model_sha256: None,
            })
        }
    }))
    .buffered(STORAGE_CONCURRENCY)
    .try_collect()
    .await?;

    Ok(PlanJobs {
        fleet_detail: fleet,
        plan_id: plan_id.clone(),
        total: manifest.job_ids.len(),
        offset,
        limit,
        jobs,
        descriptor_names: None,
        results_read: None,
        rerun_campaign_id: None,
        may_cancel: None,
        cancel_permission: None,
        may_edit_note: None,
        document_state: None,
    })
}

// GET /api/v1/plans/{plan_id}/explain
//
// `read_clients` gates the projection, not the route, so a submitter can ask why
// its own plan is stuck whatever it holds.
pub async fn explain_plan(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    // One permission read answers both gates: whether the caller may read the
    // plan, and how much of the fleet the answer may name.
    let read = PlanRead::resolve(&state, caller, None).await?;

    // The report reads the manifest again: threading this one through would fork
    // it from the CLI path, and the queue and fleet listings dominate its cost.
    scoped_manifest(&state, &read.scope, &plan_id).await?;

    let (_, report) = explain_plan_with(
        state.plan_store.as_ref(),
        state.todo_store.as_ref(),
        state.client_store.as_ref(),
        state.tag_store.as_ref(),
        &plan_id,
        Utc::now(),
    )
    .await?;

    Ok(Json(report.view(read.fleet)))
}

// POST /api/v1/plans/{plan_id}/cancel
//
// Writes the marker and stops: `queue-maintenance` owns the status latch and the
// teardown, as a status write here would race it. Hence `202` and no status.
pub async fn cancel_plan(
    State(state): State<AppState>,
    ActiveUser { user, rights, .. }: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
) -> Result<Response, AppError> {
    let (status, view) = cancel(&state, &user, &rights, &plan_id).await?;
    audit.changed(view.outcome == pipette_plan_types::CancelOutcome::Requested);
    Ok((status, Json(view)).into_response())
}

// PATCH /api/v1/plans/{plan_id}: replace a plan's note.
pub async fn update_plan(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(plan_id)): ApiPath<Parsed<PlanId>>,
    ApiJson(body): ApiJson<UpdatePlanRequest>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    // A blank note clears it, so it reads as `Some(None)`.
    let asked = body
        .note
        .map(|note| NonEmptyTrimmedString::try_new(note).ok());
    check_note(asked.as_ref().and_then(Option::as_ref))?;
    let (manifest, read) = read_plan(&state, caller, &plan_id).await?;
    let campaign = PlanCampaign::read(&state, caller.user, &read.rights, &manifest).await?;
    if !note_editable(caller.user, &read.rights, &manifest, campaign.as_ref()) {
        tracing::warn!(%plan_id, "refused: the plan's note is not the caller's to change");
        return Err(AppError::Forbidden(
            "a plan's note is changed by whoever may file into its campaign, or by its filer \
             outside one"
                .into(),
        ));
    }
    let stored = plan_notes(&state, &plan_id).await?;
    // A no-op writes nothing. A write replaces the note whatever it holds, so the
    // last write wins: a retry that lands after another editor's write replaces it.
    let changed = asked.as_ref().is_some_and(|note| {
        note.as_ref().map(NonEmptyTrimmedString::as_str) != stored.note.as_deref()
    });
    audit.changed(changed);
    let notes = match asked {
        Some(note) if changed => {
            let written = state
                .plan_store
                .set_plan_note(&plan_id, note.as_ref().map(NonEmptyTrimmedString::as_str))
                .await
                .map_err(|e| AppError::internal(e.context("failed to write the plan's note")))?;
            if !written {
                return Err(AppError::NotFound("plan not found".into()));
            }
            tracing::info!(%plan_id, "plan note changed");
            PlanNotes {
                note: note.map(Into::into),
                ..stored
            }
        }
        _ => stored,
    };
    let cancel_requested = state
        .plan_store
        .has_cancel_marker(&plan_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the cancel marker")))?;
    Ok(Json(Changed {
        record: manifest.view(notes, cancel_requested, read.fleet),
        changed,
    }))
}

/// The notes of `plan_id`, kept apart from its manifest.
pub(crate) async fn plan_notes(state: &AppState, plan_id: &PlanId) -> Result<PlanNotes, AppError> {
    state
        .plan_store
        .get_plan_notes(plan_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the plan's notes")))
}

/// The campaign of a plan and the authority of the reader over it. The plan page
/// reads them one time and uses them for each grant that it offers.
struct PlanCampaign {
    /// `None` when the record is gone.
    campaign: Option<Campaign>,
    authority: CampaignAuthority,
}

impl PlanCampaign {
    /// `None` for a plan filed outside every campaign.
    async fn read(
        state: &AppState,
        user: &User,
        rights: &UserRights,
        manifest: &PlanManifest,
    ) -> Result<Option<Self>, AppError> {
        let Some(campaign_id) = &manifest.campaign_id else {
            return Ok(None);
        };
        let (campaign, member_of) = futures::try_join!(
            async {
                state
                    .campaign_store
                    .get_campaign(campaign_id)
                    .await
                    .map_err(|e| {
                        AppError::internal(e.context("failed to read the plan's campaign"))
                    })
            },
            user_org(state, &user.user_id),
        )?;
        Ok(Some(Self {
            campaign,
            authority: CampaignAuthority::of(user, rights, member_of),
        }))
    }
}

/// Whether `user` may change the note of `manifest`. The user must hold
/// `submit_plans`, and one of these conditions must be true:
/// - The plan is in a campaign that the user may file into.
/// - The plan is outside a campaign, and the user filed it.
/// - The user holds `manage_any_orgs`. This applies to each plan that the user
///   reads, which includes a plan whose campaign record is gone.
fn note_editable(
    user: &User,
    rights: &UserRights,
    manifest: &PlanManifest,
    campaign: Option<&PlanCampaign>,
) -> bool {
    rights.holds(UserPermission::SubmitPlans)
        && match campaign {
            None => {
                manifest.submitted_by.as_ref() == Some(&user.user_id)
                    || rights.holds(UserPermission::ManageAnyOrgs)
            }
            Some(PlanCampaign {
                campaign: Some(campaign),
                authority,
            }) => authority.may_run(&campaign.owner),
            Some(PlanCampaign { campaign: None, .. }) => {
                rights.holds(UserPermission::ManageAnyOrgs)
            }
        }
}

/// The cancel grant a plan wants. Shared with the plan page, so its button and
/// [`cancel`] agree.
async fn cancel_grant(
    state: &AppState,
    user: &User,
    rights: &UserRights,
    manifest: &PlanManifest,
    campaign: Option<&PlanCampaign>,
) -> Result<UserPermission, AppError> {
    let reaches = match campaign {
        None => manifest.submitted_by.as_ref() == Some(&user.user_id),
        Some(PlanCampaign {
            campaign: Some(campaign),
            authority,
        }) => {
            authority.runs(&campaign.owner)
                || authority.offboards(state, rights, &campaign.owner).await?
        }
        Some(PlanCampaign { campaign: None, .. }) => false,
    };
    Ok(if reaches {
        UserPermission::CancelPlans
    } else {
        UserPermission::CancelAnyPlans
    })
}

/// Record a cancellation request, and what it did.
pub(crate) async fn cancel(
    state: &AppState,
    user: &User,
    rights: &UserRights,
    plan_id: &PlanId,
) -> Result<(StatusCode, PlanCancelRequested), AppError> {
    let user_id = &user.user_id;
    // The route's gate checked it already; kept before the manifest read so a
    // caller holding neither cancel grant learns nothing about the plan.
    rights.require(UserPermission::CancelPlans)?;

    let manifest = state
        .plan_store
        .get_plan(plan_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the plan manifest")))?;

    let Some(manifest) = manifest else {
        // A `cancel_plans` caller gets `403` for an absent plan too, so it
        // cannot probe for plan ids.
        rights.require(UserPermission::CancelAnyPlans)?;
        return Err(AppError::NotFound("plan not found".into()));
    };

    // Refused naming the grant this plan wants, so a holder of plain
    // `cancel_plans` is told which one it lacks.
    let campaign = PlanCampaign::read(state, user, rights, &manifest).await?;
    rights.require(cancel_grant(state, user, rights, &manifest, campaign.as_ref()).await?)?;

    let outcome = request_cancel(state.plan_store.as_ref(), &manifest)
        .await
        .map_err(|e| AppError::internal(e.context("failed to request the cancellation")))?;

    // A `complete` plan is the one refusal: nothing is left to tear down. `409`,
    // not `404`: the caller was admitted to cancel it.
    let (status, answered, message) = match outcome {
        CancelOutcome::Requested => (
            StatusCode::ACCEPTED,
            pipette_plan_types::CancelOutcome::Requested,
            "the cancel marker is written; the next queue-maintenance run tears the plan down",
        ),
        CancelOutcome::AlreadyCancelling => (
            StatusCode::ACCEPTED,
            pipette_plan_types::CancelOutcome::AlreadyCancelling,
            "teardown already owns this plan and is still retiring its jobs; nothing was written",
        ),
        CancelOutcome::AlreadyCancelled => (
            StatusCode::ACCEPTED,
            pipette_plan_types::CancelOutcome::AlreadyCancelled,
            "teardown already finished for this plan; nothing was written",
        ),
        CancelOutcome::AlreadyComplete => {
            return Err(AppError::Conflict(
                "the plan is already complete: every job reached a terminal state, \
                 so there is nothing to cancel"
                    .into(),
            ));
        }
    };

    tracing::info!(
        user_id = %user_id,
        plan_id = %plan_id,
        outcome = outcome.label(),
        "plan cancellation requested over HTTP"
    );

    // No plan in the answer: `cancel_plans` reads nothing, and a caller that
    // reads the plan asks for it.
    Ok((
        status,
        PlanCancelRequested {
            outcome: answered,
            message: message.to_string(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::defaults(None, None, None, JobWindow::Page { offset: 0, limit: JOBS_PAGE_DEFAULT })]
    #[case::passes_through(None, Some(10), Some(5), JobWindow::Page { offset: 5, limit: 10 })]
    #[case::clamps_not_refuses(None, Some(9_999), None, JobWindow::Page { offset: 0, limit: JOBS_PAGE_MAX })]
    #[case::count_only(None, Some(0), None, JobWindow::Page { offset: 0, limit: 0 })]
    #[case::all_ignores_paging(Some(true), Some(10), Some(5), JobWindow::All)]
    fn job_window_pages(
        #[case] all: Option<bool>,
        #[case] limit: Option<usize>,
        #[case] offset: Option<usize>,
        #[case] expected: JobWindow,
    ) {
        let query = PlanJobsQuery {
            all,
            limit,
            offset,
            ..PlanJobsQuery::default()
        };
        assert_eq!(job_window(&query), expected);
    }

    #[rstest]
    #[case::short("Nightly", "Nightly")]
    #[case::ascii(&"a".repeat(300), &"a".repeat(200))]
    #[case::multibyte(&"夜".repeat(100), &"夜".repeat(66))]
    fn clipped_cuts_a_long_name_between_characters(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(clipped(name), expected);
    }

    #[rstest]
    #[case::plain(
        "Nightly-plan-3",
        "attachment; filename=\"Nightly-plan-3.csv\"; filename*=UTF-8''Nightly-plan-3.csv"
    )]
    #[case::spaced(
        "calibration run-plan-3",
        "attachment; filename=\"calibration run-plan-3.csv\"; filename*=UTF-8''calibration%20run-plan-3.csv"
    )]
    #[case::unsafe_chars(
        "a/b\\c\"d\te",
        "attachment; filename=\"a_b_c_d_e.csv\"; filename*=UTF-8''a_b_c_d_e.csv"
    )]
    #[case::non_ascii(
        "Nächtlich",
        "attachment; filename=\"N_chtlich.csv\"; filename*=UTF-8''N%C3%A4chtlich.csv"
    )]
    #[case::percent(
        "100% done's",
        "attachment; filename=\"100% done's.csv\"; filename*=UTF-8''100%25%20done%27s.csv"
    )]
    #[case::invisible(
        "a\u{202E}b\u{200B}c",
        "attachment; filename=\"a_b_c.csv\"; filename*=UTF-8''a_b_c.csv"
    )]
    fn results_disposition_names_the_file(#[case] stem: &str, #[case] expected: &str) {
        assert_eq!(results_disposition(stem), expected);
    }
}
