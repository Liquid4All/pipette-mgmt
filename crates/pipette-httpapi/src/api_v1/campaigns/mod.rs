//! The campaign **writes** under `/api/v1/campaigns`; the reads live in `reads`.
//! Create, edit and hand-over take separate request shapes, so a rename cannot move a campaign
//! between owners. The pointer moves only by filing work, so a plan never has two campaigns.

pub mod reads;

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{FromRequest, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use tracing::Instrument;

use pipette_core::campaign::{Campaign, CampaignOwner, NewCampaign};
use pipette_core::plan::PlanManifest;
use pipette_pipeline::campaign_admin::{
    CampaignAdminError, Head, Offer, Revision, RevisionDryRun, inheriting,
};
use pipette_pipeline::hf_import::Progress;
use pipette_pipeline::org_catalog::PrivateRunError;
use pipette_pipeline::plan_document::{DocumentDryRun, DryRunReport};
use pipette_pipeline::plan_ingestion::CampaignMembership;
use pipette_pipeline::variant_edit::VariantEdit;
use pipette_pipeline::{campaign_admin, variant_edit};
use pipette_plan_types::{
    Answered, AuditTarget, BenchmarkId, CampaignAnswer, CampaignCreated, CampaignDocument,
    CampaignId, CampaignPlanDocumentQuery, CampaignRevised, CampaignVariants, CampaignView,
    Changed, CreateCampaignQuery, FleetDetail, JobId, NonEmptyTrimmedString, PlanAnswer, PlanFiled,
    PlanId, RecalculateJobsRequest, RerunJobsQuery, RerunJobsRequest, RevertCampaignQuery,
    RevertCampaignRequest, ReviseCampaignQuery, RevisionAnswer, RevisionBase,
    SetCampaignOwnerRequest, UpdateCampaignRequest, UserPermission, VaultPrincipalId,
};

use crate::api_v1::plans::FilingScope;
use crate::api_v1::plans::reads::plan_notes;
use crate::audit::Audit;
use crate::auth::{ActiveUser, Caller, UserRights, user_org};
use crate::campaign_access::CampaignAuthority;
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery, Parsed, PlanDocument};
use crate::owner::require_destination;
use crate::progress_stream;
use crate::state::AppState;

/// Ceiling on a campaign name, and on a vault resource's, in characters: a
/// label every record read carries.
pub(crate) const NAME_MAX_CHARS: usize = 4096;

/// The most plans a revision inherits from: each costs a walk of its campaign's
/// plans and a read of every job it holds.
const INHERIT_FROM_MAX: usize = 32;

/// What a re-run or a recalculation takes of the head: jobs by id, or every
/// job a whole-plan selector picks.
enum Selection<W> {
    Jobs(BTreeSet<JobId>),
    Whole(W),
}

/// Exactly one of `job_ids` and the whole-plan selector `name`. The selector
/// needs `base_plan_id`, and has no dry run.
fn selection<W>(
    query: &RerunJobsQuery,
    job_ids: Option<Vec<JobId>>,
    whole: Option<W>,
    name: &str,
) -> Result<Selection<W>, AppError> {
    match (job_ids, whole) {
        (Some(job_ids), None) => Ok(Selection::Jobs(job_ids.into_iter().collect())),
        (None, Some(_)) if query.base_plan_id.is_none() => Err(AppError::BadRequest(format!(
            "{name} needs base_plan_id, the plan it was read off"
        ))),
        (None, Some(_)) if query.dry_run => Err(AppError::BadRequest(format!(
            "dry_run takes job_ids, not {name}"
        ))),
        (None, Some(whole)) => Ok(Selection::Whole(whole)),
        (Some(_), Some(_)) | (None, None) => Err(AppError::BadRequest(format!(
            "name either job_ids or {name}"
        ))),
    }
}

fn base_plan_id(plan_id: PlanId) -> PlanId {
    tracing::Span::current().record("plan_id", tracing::field::display(&plan_id));
    plan_id
}

/// The one bound on a name the type does not carry.
fn check_name(name: &NonEmptyTrimmedString) -> Result<(), AppError> {
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "name must not exceed {NAME_MAX_CHARS} characters"
        )));
    }
    Ok(())
}

/// The campaign `campaign_id` names, or a `404`; `campaign_admin::require` gives
/// the CLI the same answer.
pub async fn require(state: &AppState, campaign_id: &CampaignId) -> Result<Campaign, AppError> {
    Ok(campaign_admin::require(state.campaign_store.as_ref(), campaign_id).await?)
}

/// Load a campaign to **file into**, refusing one this caller may not run,
/// with the authority that admitted it.
async fn for_plan_write(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<(CampaignAuthority, Campaign), AppError> {
    let authority = CampaignAuthority::resolve(state, caller).await?;
    let campaign = require(state, campaign_id).await?;
    authority.require_run(&campaign)?;
    Ok((authority, campaign))
}

/// Load a campaign for a write to the record itself. The authority resolves first, so a
/// caller holding no grant learns nothing of whether the campaign exists.
async fn for_record_write(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<(CampaignAuthority, Campaign), AppError> {
    let authority = CampaignAuthority::resolve(state, caller).await?;
    let campaign = require(state, campaign_id).await?;
    authority
        .require_record(state, caller.rights, &campaign)
        .await?;
    Ok((authority, campaign))
}

/// Refuse an account owner without `submit_plans`, whoever hands the campaign
/// over: without it the owner could neither run the campaign nor hand it back.
async fn require_owner_files_plans(
    state: &AppState,
    owner: &CampaignOwner,
) -> Result<(), AppError> {
    let user_id = match owner {
        CampaignOwner::User { user_id } => user_id,
        // An organization's campaigns run under its members' grants.
        CampaignOwner::Org { .. } => return Ok(()),
    };
    let owner_account = state
        .user_store
        .get_user(user_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the owner's account")))?
        .ok_or_else(|| AppError::BadRequest(format!("no account {user_id}")))?;
    if UserRights::of_account(state, &owner_account)
        .await?
        .holds(UserPermission::SubmitPlans)
    {
        return Ok(());
    }
    tracing::warn!(owner = %owner.label(), "refused: the owner holds no submit_plans");
    Err(AppError::BadRequest(format!(
        "a campaign's owner needs submit_plans, which {user_id} does not hold"
    )))
}

/// Refuse a new owner outside the organization whose private benchmark or vault resource
/// the campaign ran. One that may have been private takes `manage_any_orgs` to hand over.
async fn require_owner_holds_private_run(
    state: &AppState,
    rights: &UserRights,
    campaign: &Campaign,
    owner: &CampaignOwner,
) -> Result<(), AppError> {
    let public = state
        .catalog_cache
        .get()
        .await
        .map_err(|e| AppError::internal(e.context("failed to load the benchmark catalog")))?;
    let run = pipette_pipeline::org_catalog::private_run(
        state.catalog_store.as_ref(),
        state.plan_store.as_ref(),
        &public,
        campaign,
    )
    .await
    .map_err(|e| match e {
        PrivateRunError::UnreadableSources(_) => {
            tracing::warn!(campaign_id = %campaign.campaign_id, error = %e, "refused: what the campaign ran cannot be told");
            AppError::Conflict(format!("{e}, so it cannot be handed over"))
        }
        PrivateRunError::Store(e) => {
            AppError::internal(e.context("failed to read what the campaign ran"))
        }
    })?;
    if run.has_unknowns() && !rights.holds(UserPermission::ManageAnyOrgs) {
        tracing::warn!(campaign_id = %campaign.campaign_id, benchmarks = ?run.unresolved, jobs = run.unknown_jobs, "refused: whose results the campaign holds cannot be told");
        let benchmarks: Vec<&str> = run.unresolved.iter().map(BenchmarkId::as_str).collect();
        let unknown = [
            (!benchmarks.is_empty()).then(|| {
                format!(
                    "benchmarks no catalog holds any more ({})",
                    benchmarks.join(", ")
                )
            }),
            (run.unknown_jobs > 0).then(|| {
                format!(
                    "{} job(s) whose definition does not say what they ran",
                    run.unknown_jobs
                )
            }),
        ];
        return Err(AppError::Conflict(format!(
            "this campaign holds {}, so handing it over takes manage_any_orgs",
            unknown
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" and ")
        )));
    }
    if run.held.is_empty() && run.vault.is_empty() {
        return Ok(());
    }
    let destination = match owner {
        CampaignOwner::Org { org_id } => Some(org_id.clone()),
        CampaignOwner::User { user_id } => user_org(state, user_id).await?,
    };
    let outside = run.outside(destination.as_ref());
    if outside.is_empty() {
        return Ok(());
    }
    tracing::warn!(owner = %owner.label(), outside = ?outside, "refused: the new owner's organization does not hold what the campaign ran");
    Err(AppError::Conflict(format!(
        "this campaign ran private benchmarks or vault resources the new owner's organization \
         does not hold: {}",
        outside.into_iter().collect::<Vec<_>>().join(", ")
    )))
}

/// Run a plan write and the campaign write after it on their own task, so a
/// client that disconnects cannot cancel the handler between them and strand
/// a live plan the campaign never took up. The audit event takes the writes'
/// own outcome, which stands even when nobody hears the answer.
async fn run_to_completion<T: Send + 'static>(
    audit: &Audit,
    writes: impl Future<Output = Result<T, AppError>> + Send + 'static,
) -> Result<T, AppError> {
    let held = audit.clone();
    let writes = async move {
        let written = writes.await;
        let status = written
            .as_ref()
            .map_or_else(AppError::status, |_| StatusCode::CREATED);
        held.committed(status.as_u16());
        written
    };
    audit
        .spawn(writes.instrument(tracing::Span::current()))
        .await
        .map_err(|e| {
            AppError::internal(anyhow::Error::new(e).context("the campaign writes failed"))
        })?
}

// POST /api/v1/campaigns
pub async fn create_campaign(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiQuery(query): ApiQuery<CreateCampaignQuery>,
    request: Request,
) -> Result<Response, AppError> {
    let creator =
        CampaignCreator::authorize(&state, active.caller(), query.name, query.owner).await?;

    // Read only now, so a refused caller costs no plan-sized body.
    let stream = progress_stream::requested(request.headers());
    let PlanDocument(text) = PlanDocument::from_request(request, &state).await?;
    let held = audit.clone();
    let create = move |progress: Progress| async move {
        if query.dry_run {
            let report = creator.dry_run(&state, query.note, text).await?;
            audit.unchanged();
            return Ok((StatusCode::OK, CampaignAnswer::DryRun(report.into())));
        }
        let (campaign, report) = creator
            .create(&state, &audit, query.note, text, progress)
            .await?;
        let created = CampaignCreated {
            campaign: campaign.into(),
            plan: report,
        };
        Ok::<_, AppError>((
            StatusCode::CREATED,
            CampaignAnswer::Created(Box::new(created)),
        ))
    };
    progress_stream::answer(stream, held, create).await
}

/// A campaign this caller may create, under an owner checked before the first
/// plan's document is read.
struct CampaignCreator {
    user: pipette_core::user::User,
    new: NewCampaign,
    scope: FilingScope,
    fleet: FleetDetail,
}

impl CampaignCreator {
    /// Takes `submit_plans` too, since creating a campaign files its first plan.
    /// An absent `owner` is the caller.
    async fn authorize(
        state: &AppState,
        caller: Caller<'_>,
        name: NonEmptyTrimmedString,
        owner: Option<VaultPrincipalId>,
    ) -> Result<Self, AppError> {
        let user = caller.user;
        let authority = CampaignAuthority::resolve(state, caller).await?;
        check_name(&name)?;
        let owner = owner.map_or_else(|| authority.own_owner(), CampaignOwner::from);
        // Checked **before** anything is written, so a refused owner files no plan.
        require_destination(
            state,
            &authority,
            &owner,
            "you may only create campaigns for yourself, an organization you administer, \
             or a member of it",
        )
        .await?;

        let rights = caller.rights;
        rights.require(UserPermission::SubmitPlans)?;
        require_owner_files_plans(state, &owner).await?;
        Ok(Self {
            user: user.clone(),
            new: NewCampaign::new(name, owner),
            scope: crate::api_v1::plans::plan_scope(state, user, rights).await?,
            fleet: rights.fleet_detail(),
        })
    }

    /// Expand and validate the first plan's document, and create nothing.
    async fn dry_run(
        &self,
        state: &AppState,
        note: Option<NonEmptyTrimmedString>,
        text: String,
    ) -> Result<DryRunReport, AppError> {
        let plan =
            crate::api_v1::plans::expand_document(state, &self.scope, note.as_ref(), text).await?;
        let dry_run =
            crate::api_v1::plans::dry_run_plan(state, &self.scope, self.new.owner(), note, plan)
                .await?;
        Ok(dry_run.report)
    }

    /// File the first plan, then write the campaign around it.
    async fn create(
        self,
        state: &AppState,
        audit: &Audit,
        note: Option<NonEmptyTrimmedString>,
        text: String,
        progress: Progress,
    ) -> Result<(Campaign, PlanFiled), AppError> {
        let plan =
            crate::api_v1::plans::expand_document(state, &self.scope, note.as_ref(), text).await?;
        let Self {
            user,
            new,
            scope,
            fleet,
        } = self;
        tracing::Span::current().record("campaign_id", tracing::field::display(new.campaign_id()));
        // The import runs before the writes that must complete, so a disconnect
        // during it creates no campaign.
        let membership = CampaignMembership::first(&new);
        let prepared = crate::api_v1::plans::prepare(
            state,
            &user.user_id,
            scope,
            note,
            &membership.owner,
            plan,
            progress,
        )
        .await?;
        let state = state.clone();
        let naming = audit.clone();
        run_to_completion(audit, async move {
            // The plan first, so a failure between the writes leaves a plan naming
            // a campaign never written, not a campaign naming a plan never filed.
            let report =
                crate::api_v1::plans::file_prepared(&state, &user.user_id, membership, prepared)
                    .await?;
            let made = [
                AuditTarget::Campaign {
                    campaign_id: new.campaign_id().clone(),
                },
                AuditTarget::Plan {
                    plan_id: report.plan_id.clone(),
                },
            ];
            naming.involve(&state, made).await;
            let campaign = campaign_admin::create_around(
                state.campaign_store.as_ref(),
                state.plan_store.as_ref(),
                new,
                report.plan_id.clone(),
            )
            .await?;
            // Again, now the campaign is on record to name its owner.
            let made = AuditTarget::Campaign {
                campaign_id: campaign.campaign_id.clone(),
            };
            naming.involve(&state, [made]).await;
            Ok((campaign, report.view(fleet)))
        })
        .await
    }
}

// PATCH /api/v1/campaigns/{campaign_id}
pub async fn update_campaign(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiJson(body): ApiJson<UpdateCampaignRequest>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let (_, mut campaign) = for_record_write(&state, caller, &campaign_id).await?;
    let UpdateCampaignRequest {
        name,
        labels,
        state: new_state,
    } = body;
    if let Some(name) = &name {
        check_name(name)?;
    }
    let now = Utc::now();
    let renamed = name.is_some_and(|name| campaign.rename(name, now));
    let relabelled = labels.is_some_and(|labels| campaign.set_labels(labels, now));
    let restated = new_state.is_some_and(|new_state| campaign.set_state(new_state, now));
    let changed = renamed || relabelled || restated;
    // Nothing is written for a no-op, so a request that changes nothing cannot
    // lose a concurrent edit to a last-writer-wins PUT.
    if changed {
        state
            .campaign_store
            .put_campaign(&campaign)
            .await
            .map_err(|e| AppError::internal(e.context("failed to write the campaign")))?;
        if renamed {
            tracing::info!("campaign renamed");
        }
        if relabelled {
            tracing::info!("campaign relabelled");
        }
        if restated {
            tracing::info!(state = ?campaign.state, "campaign state set");
        }
    } else {
        audit.unchanged();
    }
    Ok(Json(Changed {
        record: view(campaign),
        changed,
    }))
}

// PUT /api/v1/campaigns/{campaign_id}/owner
pub async fn set_campaign_owner(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiJson(body): ApiJson<SetCampaignOwnerRequest>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let (campaign, changed) = hand_over(&state, caller, &campaign_id, body.owner).await?;
    if !changed {
        audit.unchanged();
    }
    Ok(Json(Changed {
        record: view(campaign),
        changed,
    }))
}

/// Hand a campaign to `owner`, checked at both ends, and say whether anything
/// changed.
async fn hand_over(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
    owner: CampaignOwner,
) -> Result<(Campaign, bool), AppError> {
    // The campaign must be this caller's to give before the destination is
    // considered, so a refused hand-off discloses nothing about where it was headed.
    let (authority, mut campaign) = for_record_write(state, caller, campaign_id).await?;
    require_destination(
        state,
        &authority,
        &owner,
        "you may only hand a campaign to yourself, an organization you administer or belong to, \
         or a member of the organization you administer",
    )
    .await?;
    require_owner_files_plans(state, &owner).await?;
    if owner != campaign.owner {
        require_owner_holds_private_run(state, caller.rights, &campaign, &owner).await?;
    }

    let changed = campaign.set_owner(owner, Utc::now());
    if changed {
        state
            .campaign_store
            .put_campaign(&campaign)
            .await
            .map_err(|e| AppError::internal(e.context("failed to write the campaign")))?;
        tracing::info!(owner = %campaign.owner.label(), "campaign owner set");
    }
    Ok((campaign, changed))
}

// POST /api/v1/campaigns/{campaign_id}/plans
pub async fn file_plan(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<CampaignPlanDocumentQuery>,
    request: Request,
) -> Result<Response, AppError> {
    // Checked before anything is ingested.
    let (_, campaign) = for_plan_write(&state, active.caller(), &campaign_id).await?;
    campaign_admin::require_active(&campaign)?;

    // Attribution is the caller, never the campaign's owner: a team campaign
    // records which colleague filed each revision.
    let ActiveUser { user, rights, .. } = active;
    rights.require(UserPermission::SubmitPlans)?;
    let fleet = rights.fleet_detail();

    // Read only now, so a refused caller costs no plan-sized body.
    let stream = progress_stream::requested(request.headers());
    let PlanDocument(text) = PlanDocument::from_request(request, &state).await?;
    let CampaignPlanDocumentQuery { note, dry_run } = query;
    let held = audit.clone();
    let file = move |progress: Progress| async move {
        let scope = crate::api_v1::plans::plan_scope(&state, &user, &rights).await?;
        let plan =
            crate::api_v1::plans::expand_document(&state, &scope, note.as_ref(), text).await?;
        if dry_run {
            let dry_run =
                crate::api_v1::plans::dry_run_plan(&state, &scope, &campaign.owner, note, plan)
                    .await?;
            audit.unchanged();
            return Ok((StatusCode::OK, PlanAnswer::DryRun(dry_run.report.into())));
        }

        // The import runs before the writes that must complete, so a disconnect
        // during it files no plan.
        let membership = CampaignMembership::of(&campaign);
        let prepared = crate::api_v1::plans::prepare(
            &state,
            &user.user_id,
            scope,
            note,
            &membership.owner,
            plan,
            progress,
        )
        .await?;
        // The import can take hours: refuse the plan if the campaign changed owner
        // or head meanwhile.
        let as_filer = Caller {
            user: &user,
            rights: &rights,
        };
        let (_, current) = for_plan_write(&state, as_filer, &campaign_id).await?;
        if campaign_admin::changed_under(&campaign, &current) {
            tracing::warn!("campaign plan refused: the campaign changed during the import");
            return Err(AppError::Conflict(
                "the campaign changed its owner or its plan while the plan was prepared. \
                 File the plan again"
                    .to_string(),
            ));
        }
        let committing = audit.clone();
        let report = run_to_completion(&committing, async move {
            // The plan before the advance: a failure between leaves a revision that
            // never took effect, not a campaign pointing at a plan that may not exist.
            let report =
                crate::api_v1::plans::file_prepared(&state, &user.user_id, membership, prepared)
                    .await?;
            audit
                .involve(
                    &state,
                    [AuditTarget::Plan {
                        plan_id: report.plan_id.clone(),
                    }],
                )
                .await;

            // The record above predates the ingest, so the advance re-reads it
            // rather than writing that copy back.
            campaign_admin::advance(
                state.campaign_store.as_ref(),
                state.plan_store.as_ref(),
                &campaign_id,
                report.plan_id.clone(),
            )
            .await?;
            Ok(report.view(fleet))
        })
        .await?;
        Ok::<_, AppError>((StatusCode::CREATED, PlanAnswer::Filed(report)))
    };
    progress_stream::answer(stream, held, file).await
}

// POST /api/v1/campaigns/{campaign_id}/variants
pub async fn edit_variants(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<ReviseCampaignQuery>,
    request: Request,
) -> Result<Response, AppError> {
    let caller = active.caller();
    let base = base_plan_id(query.base_plan_id);
    let reviser =
        CampaignReviser::authorize(&state, caller, &campaign_id, Some(&base), &audit).await?;
    let add_from = query.add_from.unwrap_or_default();
    let restore = query.restore.unwrap_or_default();
    let inherit_from = inheriting(&query.inherit_from.unwrap_or_default(), &add_from, &restore);
    let offer = offer(&state, caller, query.carry_earlier, &inherit_from).await?;

    // Read only now, so a refused caller costs no body.
    let stream = progress_stream::requested(request.headers());
    let PlanDocument(text) = PlanDocument::from_request(request, &state).await?;
    let held = audit.clone();
    let edit_variants = move |progress: Progress| async move {
        let added = offer
            .variants_of(state.plan_store.as_ref(), &add_from)
            .await?;
        let restored = offer
            .restored_variants(state.plan_store.as_ref(), &restore)
            .await?;
        let edit = VariantEdit::parse(&text)
            .map_err(CampaignAdminError::from)?
            .adding(added)
            .restoring(restored, restore);
        if query.dry_run {
            let report = reviser.dry_run(&state, &edit, query.note, offer).await?;
            audit.unchanged();
            return Ok((StatusCode::OK, RevisionAnswer::DryRun(report.into())));
        }
        let revised = reviser
            .file(&state, &edit, query.note, offer, progress)
            .await?;
        Ok::<_, AppError>((StatusCode::CREATED, RevisionAnswer::Revised(revised)))
    };
    progress_stream::answer(stream, held, edit_variants).await
}

// GET /api/v1/campaigns/{campaign_id}/variants
//
// Behind the edit's own gate, so a form is offered only to a caller the edit
// would admit.
pub async fn list_variants(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let reviser =
        CampaignReviser::authorize(&state, caller, &campaign_id, None, &Audit::default()).await?;
    Ok(Json(CampaignVariants {
        base: reviser.base(&state).await?,
        variants: reviser.variants(&state).await?,
    }))
}

// PUT /api/v1/campaigns/{campaign_id}/document
//
// Idempotent: the head's own document, or a retry whose first attempt landed,
// files nothing.
pub async fn revise_document(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<ReviseCampaignQuery>,
    request: Request,
) -> Result<Response, AppError> {
    let caller = active.caller();
    let base = base_plan_id(query.base_plan_id);
    // The base is checked after the body is read, since a retry that landed is
    // told apart by its document.
    let reviser = CampaignReviser::authorize(&state, caller, &campaign_id, None, &audit).await?;
    if query.add_from.is_some() {
        return Err(AppError::BadRequest(
            "add_from appends variants, so it is a parameter of a variant edit".into(),
        ));
    }
    if query.restore.is_some() {
        return Err(AppError::BadRequest(
            "restore appends variants, so it is a parameter of a variant edit".into(),
        ));
    }
    let inherit_from = query.inherit_from.unwrap_or_default();
    let offer = offer(&state, caller, query.carry_earlier, &inherit_from).await?;

    // Read only now, so a refused caller costs no plan-sized body.
    let stream = progress_stream::requested(request.headers());
    let PlanDocument(text) = PlanDocument::from_request(request, &state).await?;
    let held = audit.clone();
    let revise = move |progress: Progress| async move {
        if let Err(stale) = reviser.require_base(&base) {
            if query.dry_run || !reviser.landed(&state, &base, &text).await? {
                return Err(stale);
            }
            audit.unchanged();
            return Ok((StatusCode::OK, unchanged(reviser.campaign())));
        }
        if query.dry_run {
            let report = reviser
                .dry_run_document(&state, text, query.note, offer)
                .await?;
            audit.unchanged();
            return Ok((StatusCode::OK, RevisionAnswer::DryRun(report.into())));
        }
        let campaign = reviser.campaign().clone();
        let Some(revised) = reviser
            .file_document(&state, text, query.note, offer, progress)
            .await?
        else {
            audit.unchanged();
            return Ok((StatusCode::OK, unchanged(&campaign)));
        };
        Ok::<_, AppError>((StatusCode::CREATED, RevisionAnswer::Revised(revised)))
    };
    progress_stream::answer(stream, held, revise).await
}

/// What a revision offers beside the head: the campaign's earlier plans and each
/// `inherit_from` plan. One the caller may not read is a `404`.
async fn offer(
    state: &AppState,
    caller: Caller<'_>,
    earlier: bool,
    inherit_from: &[PlanId],
) -> Result<Offer, AppError> {
    if inherit_from.len() > INHERIT_FROM_MAX {
        return Err(AppError::BadRequest(format!(
            "inherit_from, add_from and restore name {} plans; a revision inherits from at most {INHERIT_FROM_MAX}",
            inherit_from.len()
        )));
    }
    let offer = Offer::read(
        state.plan_store.as_ref(),
        state.campaign_store.as_ref(),
        earlier,
        inherit_from,
    )
    .await?;
    let manifests: Vec<&PlanManifest> = offer.plans().iter().collect();
    let readable = crate::api_v1::plans::reads::readable(state, caller, &manifests).await?;
    if let Some((manifest, _)) = manifests
        .iter()
        .zip(readable)
        .find(|(_, readable)| !readable)
    {
        tracing::warn!(plan_id = %manifest.plan_id, "refused: the plan to inherit from is outside the caller's scope");
        return Err(AppError::OutOfScope(
            CampaignAdminError::NoSuchPlan(manifest.plan_id.clone()).to_string(),
        ));
    }
    Ok(offer)
}

/// The `200` answer of a document revision that files nothing, as a rename to
/// the same name answers.
fn unchanged(campaign: &Campaign) -> RevisionAnswer {
    RevisionAnswer::Unchanged(Changed {
        record: view(campaign.clone()),
        changed: false,
    })
}

/// A write's answer: the record alone, as an unasked read answers it.
fn view(campaign: Campaign) -> CampaignView {
    CampaignView {
        campaign: campaign.into(),
        plan: None,
    }
}

// GET /api/v1/campaigns/{campaign_id}/document
pub async fn read_document(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let reviser =
        CampaignReviser::authorize(&state, caller, &campaign_id, None, &Audit::default()).await?;
    Ok(Json(CampaignDocument {
        base: reviser.base(&state).await?,
        document: reviser.document(&state).await?,
    }))
}

// POST /api/v1/campaigns/{campaign_id}/rerun
pub async fn rerun_jobs(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<RerunJobsQuery>,
    request: Request,
) -> Result<Response, AppError> {
    let caller = active.caller();
    let base = query.base_plan_id.clone().map(base_plan_id);
    let reviser =
        CampaignReviser::authorize(&state, caller, &campaign_id, base.as_ref(), &audit).await?;

    // Read only now, so a refused caller costs no body.
    let ApiJson(body) = ApiJson::<RerunJobsRequest>::from_request(request, &state).await?;
    let response = match selection(&query, body.job_ids, body.answered, "answered")? {
        Selection::Jobs(job_ids) if query.dry_run => {
            let report = reviser.dry_run_rerun(&state, job_ids, body.note).await?;
            audit.unchanged();
            return Ok(Json(pipette_plan_types::RevisionDryRun::from(report)).into_response());
        }
        Selection::Jobs(job_ids) => reviser.rerun(&state, job_ids, body.note).await?,
        Selection::Whole(which) => reviser.rerun_answered(&state, which, body.note).await?,
    };
    Ok((StatusCode::CREATED, Json(response)).into_response())
}

// POST /api/v1/campaigns/{campaign_id}/recalculate
pub async fn recalculate_jobs(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<RerunJobsQuery>,
    request: Request,
) -> Result<Response, AppError> {
    let caller = active.caller();
    let base = query.base_plan_id.clone().map(base_plan_id);
    let reviser =
        CampaignReviser::authorize(&state, caller, &campaign_id, base.as_ref(), &audit).await?;

    // Read only now, so a refused caller costs no body.
    let ApiJson(body) = ApiJson::<RecalculateJobsRequest>::from_request(request, &state).await?;
    let all_evals = body.all_evals.then_some(());
    let response = match selection(&query, body.job_ids, all_evals, "all_evals")? {
        Selection::Jobs(job_ids) if query.dry_run => {
            let report = reviser
                .dry_run_recalculate(&state, job_ids, body.note)
                .await?;
            audit.unchanged();
            return Ok(Json(pipette_plan_types::RevisionDryRun::from(report)).into_response());
        }
        Selection::Jobs(job_ids) => reviser.recalculate(&state, job_ids, body.note).await?,
        Selection::Whole(()) => reviser.recalculate_evals(&state, body.note).await?,
    };
    Ok((StatusCode::CREATED, Json(response)).into_response())
}

// POST /api/v1/campaigns/{campaign_id}/revert
//
// A revert to the head's own document files nothing.
pub async fn revert_campaign(
    State(state): State<AppState>,
    active: ActiveUser,
    audit: Audit,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
    ApiQuery(query): ApiQuery<RevertCampaignQuery>,
    request: Request,
) -> Result<Response, AppError> {
    let caller = active.caller();
    let base = base_plan_id(query.base_plan_id);
    let reviser =
        CampaignReviser::authorize(&state, caller, &campaign_id, Some(&base), &audit).await?;

    // Read only now, so a refused caller costs no body.
    let ApiJson(body) = ApiJson::<RevertCampaignRequest>::from_request(request, &state).await?;
    if query.dry_run {
        let report = reviser
            .dry_run_revert(&state, &body.plan_id, body.note)
            .await?;
        audit.unchanged();
        return Ok((StatusCode::OK, Json(RevisionAnswer::DryRun(report.into()))).into_response());
    }
    let campaign = reviser.campaign().clone();
    let Some(revised) = reviser.revert(&state, &body.plan_id, body.note).await? else {
        audit.unchanged();
        return Ok((StatusCode::OK, Json(unchanged(&campaign))).into_response());
    };
    Ok((StatusCode::CREATED, Json(RevisionAnswer::Revised(revised))).into_response())
}

/// A revision this caller may file of the plan a campaign runs, checked before
/// the request body is read.
pub struct CampaignReviser {
    head: Head,
    filer: Filer,
}

/// Who files a revision, and what the filing may see.
struct Filer {
    user: pipette_core::user::User,
    rights: UserRights,
    scope: FilingScope,
    fleet: FleetDetail,
    audit: Audit,
}

impl CampaignReviser {
    /// A revision reads and ends the head, so takes its plan grants and the full
    /// tier. `base` is the plan the caller read its variant indices from.
    /// `audit` is the request's event, which names what the revision files; a
    /// read passes `Audit::default()`.
    pub async fn authorize(
        state: &AppState,
        caller: Caller<'_>,
        campaign_id: &CampaignId,
        base: Option<&PlanId>,
        audit: &Audit,
    ) -> Result<Self, AppError> {
        let user = caller.user;
        let (authority, campaign) = for_plan_write(state, caller, campaign_id).await?;
        let rights = caller.rights;
        rights.require(UserPermission::SubmitPlans)?;
        let head = Head::read(state.plan_store.as_ref(), campaign, base).await?;
        head_grants(&authority, head.campaign(), head.manifest())
            .into_iter()
            .try_for_each(|grant| rights.require(grant))?;
        crate::api_v1::plans::require_document_detail(
            rights.fleet_detail(),
            &head.manifest().plan_id,
        )?;
        Ok(Self {
            head,
            filer: Filer {
                user: user.clone(),
                rights: rights.clone(),
                scope: crate::api_v1::plans::plan_scope(state, user, rights).await?,
                fleet: rights.fleet_detail(),
                audit: audit.clone(),
            },
        })
    }

    /// The head's manifest when [`Self::authorize`] would admit the caller. Tests
    /// the grants rather than requiring them, so a page logs no refusal.
    pub async fn revisable_head(
        state: &AppState,
        caller: Caller<'_>,
        campaign: &Campaign,
    ) -> Result<Option<PlanManifest>, AppError> {
        let Some(head) = &campaign.current_plan_id else {
            return Ok(None);
        };
        if !campaign.state.is_active() {
            return Ok(None);
        }
        let rights = caller.rights;
        if !rights.holds(UserPermission::SubmitPlans)
            || !crate::api_v1::plans::discloses_document(rights.fleet_detail())
        {
            return Ok(None);
        }
        let member_of = user_org(state, &caller.user.user_id).await?;
        let authority = CampaignAuthority::of(caller.user, rights, member_of);
        if !authority.may_run(&campaign.owner) {
            return Ok(None);
        }
        let head = state
            .plan_store
            .get_plan(head)
            .await
            .map_err(|e| AppError::internal(e.context("failed to read the plan manifest")))?;
        Ok(head.filter(|head| {
            head.source_sha256.is_some()
                && head_grants(&authority, campaign, head)
                    .into_iter()
                    .all(|grant| rights.holds(grant))
        }))
    }

    pub fn campaign(&self) -> &Campaign {
        self.head.campaign()
    }

    /// The plan the revision is read from, with its notes.
    pub async fn base(&self, state: &AppState) -> Result<RevisionBase, AppError> {
        let manifest = self.head.manifest();
        let notes = plan_notes(state, &manifest.plan_id).await?;
        Ok(manifest.revision_base(notes))
    }

    /// Refuse an edit whose indices were read off another plan than the head.
    pub fn require_base(&self, base: &PlanId) -> Result<(), AppError> {
        Ok(self.head.require_base(base)?)
    }

    /// The document the head was filed from.
    pub async fn document(&self, state: &AppState) -> Result<String, AppError> {
        Ok(self.head.document(state.plan_store.as_ref()).await?)
    }

    /// Whether the head is the revision of `base` filed from `document`.
    pub async fn landed(
        &self,
        state: &AppState,
        base: &PlanId,
        document: &str,
    ) -> Result<bool, AppError> {
        Ok(self
            .head
            .landed(state.plan_store.as_ref(), base, document)
            .await?)
    }

    /// The head's variants, one `[[variants]]` block each, in the order
    /// `remove` numbers them.
    pub async fn variants(&self, state: &AppState) -> Result<Vec<String>, AppError> {
        let document = self.head.document(state.plan_store.as_ref()).await?;
        Ok(variant_edit::variant_blocks(&document).map_err(CampaignAdminError::from)?)
    }

    /// Apply `edit` and expand the revised document, and file nothing.
    pub async fn dry_run(
        self,
        state: &AppState,
        edit: &VariantEdit,
        note: Option<NonEmptyTrimmedString>,
        offer: Offer,
    ) -> Result<RevisionDryRun, AppError> {
        let revision = self
            .head
            .revise(state.plan_store.as_ref(), edit)
            .await?
            .carrying(offer);
        self.filer.dry_run(state, revision, note).await
    }

    /// Expand `document` as a revision of the head, and file nothing.
    pub async fn dry_run_document(
        self,
        state: &AppState,
        document: String,
        note: Option<NonEmptyTrimmedString>,
        offer: Offer,
    ) -> Result<RevisionDryRun, AppError> {
        let revision = self
            .head
            .replace(state.plan_store.as_ref(), document)
            .await?
            .carrying(offer);
        self.filer.dry_run(state, revision, note).await
    }

    /// File `document` in place of the head's, advance the campaign onto it,
    /// and cancel the head. `None` when the document is the head's own, which
    /// files nothing.
    pub async fn file_document(
        self,
        state: &AppState,
        document: String,
        note: Option<NonEmptyTrimmedString>,
        offer: Offer,
        progress: Progress,
    ) -> Result<Option<CampaignRevised>, AppError> {
        let revision = self
            .head
            .replace(state.plan_store.as_ref(), document)
            .await?
            .carrying(offer);
        if revision.changes_nothing() {
            return Ok(None);
        }
        self.filer
            .file(state, revision, note, progress)
            .await
            .map(Some)
    }

    /// Check `job_ids` as [`Self::rerun`] does and expand the head's document,
    /// and file nothing.
    pub async fn dry_run_rerun(
        self,
        state: &AppState,
        job_ids: BTreeSet<JobId>,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<RevisionDryRun, AppError> {
        let revision = self
            .head
            .rerun(
                state.plan_store.as_ref(),
                state.submission_store.as_ref(),
                job_ids,
            )
            .await?;
        self.filer.dry_run(state, revision, note).await
    }

    /// File the variant edit, advance the campaign onto it, and cancel the head.
    pub async fn file(
        self,
        state: &AppState,
        edit: &VariantEdit,
        note: Option<NonEmptyTrimmedString>,
        offer: Offer,
        progress: Progress,
    ) -> Result<CampaignRevised, AppError> {
        let revision = self
            .head
            .revise(state.plan_store.as_ref(), edit)
            .await?
            .carrying(offer);
        self.filer.file(state, revision, note, progress).await
    }

    /// File the head's document running `job_ids` again, advance the campaign
    /// onto it, and cancel the head.
    pub async fn rerun(
        self,
        state: &AppState,
        job_ids: BTreeSet<JobId>,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<CampaignRevised, AppError> {
        let revision = self
            .head
            .rerun(
                state.plan_store.as_ref(),
                state.submission_store.as_ref(),
                job_ids,
            )
            .await?;
        self.filer
            .file(state, revision, note, Progress::Silent)
            .await
    }

    /// Check a revert to `target` as [`Self::revert`] does and expand its
    /// document, and file nothing.
    pub async fn dry_run_revert(
        self,
        state: &AppState,
        target: &PlanId,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<RevisionDryRun, AppError> {
        let revision = self.head.revert(state.plan_store.as_ref(), target).await?;
        self.filer.dry_run(state, revision, note).await
    }

    /// File the document of `target`, an earlier plan of the campaign, advance
    /// the campaign onto it, and cancel the head. `None` when the document is
    /// the head's own, which files nothing.
    pub async fn revert(
        self,
        state: &AppState,
        target: &PlanId,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<Option<CampaignRevised>, AppError> {
        let revision = self.head.revert(state.plan_store.as_ref(), target).await?;
        if revision.changes_nothing() {
            return Ok(None);
        }
        self.filer
            .file(state, revision, note, Progress::Silent)
            .await
            .map(Some)
    }

    /// Check `job_ids` as [`Self::recalculate`] does and expand the head's
    /// document, and file nothing.
    pub async fn dry_run_recalculate(
        self,
        state: &AppState,
        job_ids: BTreeSet<JobId>,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<RevisionDryRun, AppError> {
        let revision = self
            .head
            .recalculate(
                state.plan_store.as_ref(),
                state.submission_store.as_ref(),
                job_ids,
            )
            .await?;
        self.filer.dry_run(state, revision, note).await
    }

    /// File the head's document answering new jobs for `job_ids` from their
    /// records, advance the campaign onto it, and cancel the head.
    pub async fn recalculate(
        self,
        state: &AppState,
        job_ids: BTreeSet<JobId>,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<CampaignRevised, AppError> {
        let revision = self
            .head
            .recalculate(
                state.plan_store.as_ref(),
                state.submission_store.as_ref(),
                job_ids,
            )
            .await?;
        self.filer
            .file(state, revision, note, Progress::Silent)
            .await
    }

    /// [`Self::recalculate`] over every finished eval job of the head.
    pub async fn recalculate_evals(
        self,
        state: &AppState,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<CampaignRevised, AppError> {
        let revision = self
            .head
            .recalculate_evals(state.plan_store.as_ref(), state.submission_store.as_ref())
            .await?;
        self.filer
            .file(state, revision, note, Progress::Silent)
            .await
    }

    /// [`Self::rerun`] over every answered job of the head `which` takes.
    pub async fn rerun_answered(
        self,
        state: &AppState,
        which: Answered,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<CampaignRevised, AppError> {
        let revision = self
            .head
            .rerun_answered(
                state.plan_store.as_ref(),
                state.submission_store.as_ref(),
                which,
            )
            .await?;
        self.filer
            .file(state, revision, note, Progress::Silent)
            .await
    }
}

impl Filer {
    /// Run every check filing `revision` would, and file nothing. The head's own
    /// document is only expanded, with no other check and no import.
    async fn dry_run(
        &self,
        state: &AppState,
        revision: Revision,
        note: Option<NonEmptyTrimmedString>,
    ) -> Result<RevisionDryRun, AppError> {
        let plan = crate::api_v1::plans::expand_document(
            state,
            &self.scope,
            note.as_ref(),
            revision.document().into(),
        )
        .await?;
        if revision.changes_nothing() {
            return RevisionDryRun::unchanged(revision.document(), &plan.jobs, plan.plan_warnings)
                .map_err(|e| AppError::internal(anyhow::Error::new(e)));
        }
        let membership = revision
            .membership(state.plan_store.as_ref())
            .await
            .map_err(AppError::internal)?;
        let DocumentDryRun {
            report,
            preview_bodies,
        } = crate::api_v1::plans::dry_run_plan(state, &self.scope, &membership.owner, note, plan)
            .await?;
        let preview = revision
            .preview(
                membership,
                &preview_bodies,
                state.plan_store.as_ref(),
                state.submission_store.as_ref(),
                state.org_store.as_ref(),
            )
            .await?;
        RevisionDryRun::new(revision.document(), report, preview)
            .map_err(|e| AppError::internal(anyhow::Error::new(e)))
    }

    /// File `revision`, advance the campaign onto it, and cancel the head.
    async fn file(
        self,
        state: &AppState,
        revision: Revision,
        note: Option<NonEmptyTrimmedString>,
        progress: Progress,
    ) -> Result<CampaignRevised, AppError> {
        let plan = crate::api_v1::plans::expand_document(
            state,
            &self.scope,
            note.as_ref(),
            revision.document().into(),
        )
        .await?;
        let Self {
            user,
            rights,
            scope,
            fleet,
            audit,
        } = self;
        let membership = revision
            .membership(state.plan_store.as_ref())
            .await
            .map_err(AppError::internal)?;
        // The import runs before the writes that must complete, so a disconnect
        // during it files nothing. A stored document names imported models by
        // vault arm, so a re-run imports nothing.
        let prepared = crate::api_v1::plans::prepare(
            state,
            &user.user_id,
            scope,
            note,
            &membership.owner,
            plan,
            progress,
        )
        .await?
        .revising(revision.change());
        // Refuse the revision if the campaign changed owner or head during the
        // import, or the filer left the organization.
        let as_filer = Caller {
            user: &user,
            rights: &rights,
        };
        let (_, current) = for_plan_write(state, as_filer, revision.campaign_id()).await?;
        if revision.changed_under(&current) {
            tracing::warn!("campaign revision refused: the campaign changed during the import");
            return Err(AppError::Conflict(
                "the campaign changed its owner or its plan while the revision was prepared. \
                 Revise the campaign again"
                    .to_string(),
            ));
        }
        let state = state.clone();
        let naming = audit.clone();
        run_to_completion(&audit, async move {
            // The plan, then the advance, then the cancel of the replaced plan.
            let report =
                crate::api_v1::plans::file_prepared(&state, &user.user_id, membership, prepared)
                    .await?;
            naming
                .involve(
                    &state,
                    [AuditTarget::Plan {
                        plan_id: report.plan_id.clone(),
                    }],
                )
                .await;
            let revised = campaign_admin::complete_revision(
                state.campaign_store.as_ref(),
                state.plan_store.as_ref(),
                revision,
                &report.plan_id,
                &report.job_ids,
            )
            .await?;
            Ok(CampaignRevised {
                revised: revised.into(),
                plan: report.view(fleet),
            })
        })
        .await
    }
}

/// The grants to read `head`'s document and cancel it: a revision does both.
fn head_grants(
    authority: &CampaignAuthority,
    campaign: &Campaign,
    head: &PlanManifest,
) -> [UserPermission; 2] {
    if authority.runs(&campaign.owner) && head.campaign_id.as_ref() == Some(&campaign.campaign_id) {
        [UserPermission::ReadPlans, UserPermission::CancelPlans]
    } else {
        [UserPermission::ReadAnyPlans, UserPermission::CancelAnyPlans]
    }
}

// DELETE /api/v1/campaigns/{campaign_id}
//
// Deletes the pointer only: every plan the campaign filed lives on.
pub async fn delete_campaign(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(campaign_id)): ApiPath<Parsed<CampaignId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let (_, campaign) = for_record_write(&state, caller, &campaign_id).await?;
    campaign_admin::delete(&*state.campaign_store, &*state.plan_store, &campaign).await?;
    Ok(StatusCode::NO_CONTENT)
}
