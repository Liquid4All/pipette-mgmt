//! The steps that file a plan document. The campaign routes
//! ([`crate::api_v1::campaigns`]) run them. `POST /api/v1/plans/validate` runs
//! the same checks on a document and files nothing.

pub(crate) mod csv;
mod job_results;
pub mod reads;
pub(crate) mod results_cache;

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use chrono::Utc;

use pipette_core::benchmark::Benchmark;
use pipette_core::plan::PlanSource;
use pipette_core::user::User;
use pipette_core::vault::VaultOwner;
use pipette_pipeline::hf_import::{HfImports, Progress};
use pipette_pipeline::model_params::ModelParamTable;
use pipette_pipeline::org_catalog::CatalogScope;
use pipette_pipeline::plan_document::{DocumentDryRun, ExpandedDocument, index_labels};
use pipette_pipeline::plan_ingestion::{
    CampaignMembership, IngestContext, IngestError, IngestReport, ModelCheck, PlanSubmission,
    PreparedJobs, RejectionError, ingest_prepared, prepare_jobs,
};
use pipette_pipeline::plan_validation::{IssueSeverity, Stage};
use pipette_pipeline::vault_check::VaultScope;
use pipette_pipeline::{org_catalog, plan_document, plan_validation};
use pipette_plan_types::{
    BenchmarkId, FleetDetail, NonEmptyTrimmedString, PlanValidation, UserId, UserPermission,
};

use crate::audit::Audit;
use crate::auth::{PlanUser, UserRights, user_org};
use crate::error::AppError;
use crate::extract::PlanDocument;
use crate::state::AppState;

/// The maximum length of the `note` of a plan, in characters. Every plan listing
/// returns the note, so this limit keeps the listings small.
pub(crate) const NOTE_MAX_CHARS: usize = 4096;

/// Whose private benchmarks and vault resources a plan a user files may name.
pub(crate) struct FilingScope {
    catalog: CatalogScope,
    vault: VaultScope,
}

/// The [`FilingScope`] of a user: everyone's under `submit_any_plans`, else the
/// user's own and its organization's. Every door has already required it active.
pub(crate) async fn plan_scope(
    state: &AppState,
    user: &User,
    rights: &UserRights,
) -> Result<FilingScope, AppError> {
    if rights.holds(UserPermission::SubmitAnyPlans) {
        return Ok(FilingScope {
            catalog: CatalogScope::Every,
            vault: VaultScope::Every,
        });
    }
    let org = user_org(state, &user.user_id).await?;
    Ok(FilingScope {
        catalog: CatalogScope::Org(org.clone()),
        vault: VaultScope::Own {
            user: user.user_id.clone(),
            org,
        },
    })
}

// POST /api/v1/plans/validate: read in the caller's `FilingScope`, as filing reads, so no
// answer reveals another organization's catalog; the log holds counts, never a message,
// which can quote the document.
pub async fn validate_plan(
    State(state): State<AppState>,
    PlanUser { user, rights }: PlanUser,
    audit: Audit,
    PlanDocument(text): PlanDocument,
) -> Result<Json<PlanValidation>, AppError> {
    // A check writes nothing; a refusal is still recorded.
    audit.unchanged();
    let scope = plan_scope(&state, &user, &rights).await?;
    let catalogs = IngestCatalogs::load_without_models(&state, &scope).await?;
    let types = plan_document::benchmark_types(&catalogs.benchmarks);
    let stage = plan_validation::expand(state.config.plan_max_jobs, types, text)
        .await
        .map_err(|e| AppError::internal(anyhow::Error::new(e)))?;
    let validation = match stage {
        Stage::Refused(validation) => validation,
        Stage::Expanded { text, expansion } => {
            plan_validation::check(
                &catalogs.context(&state, &state.hf_imports),
                text,
                expansion,
                Utc::now(),
            )
            .await?
        }
    };
    let errors = validation
        .issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .count();
    tracing::debug!(
        jobs = validation.job_count,
        errors,
        warnings = validation.issues.len() - errors,
        "plan document validated"
    );
    Ok(Json(PlanValidation::from(validation)))
}

/// Parse and expand a plan document into the job bodies it stands for. A fault
/// in the document answers `400` with the whole chain of its messages.
pub(crate) async fn expand_document(
    state: &AppState,
    scope: &FilingScope,
    note: Option<&NonEmptyTrimmedString>,
    text: String,
) -> Result<ExpandedDocument, AppError> {
    let note = note.map(NonEmptyTrimmedString::as_str);
    let catalogs = IngestCatalogs::load_without_models(state, scope).await?;
    let types = plan_document::benchmark_types(&catalogs.benchmarks);
    plan_document::expand_document(state.config.plan_max_jobs, types, note, text)
        .await
        .map_err(|e| {
            if e.is_caller_fault() {
                AppError::BadRequest(format!("{:#}", anyhow::Error::new(e)))
            } else {
                AppError::internal(anyhow::Error::new(e))
            }
        })
}

/// Run every check a submission runs before it downloads a model, and stage
/// nothing.
pub(crate) async fn dry_run_plan(
    state: &AppState,
    scope: &FilingScope,
    owner: &VaultOwner,
    note: Option<NonEmptyTrimmedString>,
    plan: ExpandedDocument,
) -> Result<DocumentDryRun, AppError> {
    check_note(note.as_ref())?;
    // Over HTTP the model parameter table only warns, and only filing reports those warnings.
    let catalogs = IngestCatalogs::load_without_models(state, scope).await?;
    Ok(plan_document::dry_run(
        &catalogs.context(state, &state.hf_imports),
        owner,
        plan.jobs,
        plan.plan_warnings,
        chrono::Utc::now(),
    )
    .await?)
}

/// Whether a caller at `fleet` may read a stored plan document, or revise one.
/// A page tests this to hide what [`require_document_detail`] refuses.
pub fn discloses_document(fleet: FleetDetail) -> bool {
    fleet == FleetDetail::Full
}

/// Refuse to serve `plan_id`'s stored plan document, or a revision of it, to a
/// caller at the withheld tier. A redacted copy would not round-trip, so the
/// tier gates the whole document, for its filer too.
pub fn require_document_detail(
    fleet: FleetDetail,
    plan_id: &pipette_plan_types::PlanId,
) -> Result<(), AppError> {
    if discloses_document(fleet) {
        return Ok(());
    }
    tracing::warn!(
        plan_id = %plan_id,
        fleet_detail = fleet.label(),
        "refused: the plan document is withheld below read_clients"
    );
    Err(AppError::Forbidden(
        "reading a plan document needs read_clients or read_any_clients".into(),
    ))
}

/// The bounds on a note that the type does not carry: a length limit, and no NUL
/// character, which a PostgreSQL `text` column refuses.
pub(crate) fn check_note(note: Option<&NonEmptyTrimmedString>) -> Result<(), AppError> {
    let Some(note) = note else {
        return Ok(());
    };
    if note.chars().count() > NOTE_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "note is longer than {NOTE_MAX_CHARS} characters"
        )));
    }
    if note.contains('\0') {
        return Err(AppError::BadRequest(
            "note must not contain a NUL character".into(),
        ));
    }
    Ok(())
}

/// The caller's own rejection in `e`, and the index of the job it names. `None`
/// for a failure that is not the caller's, which is logged where it is raised.
fn caller_rejection(e: &IngestError) -> Option<(&RejectionError, Option<usize>)> {
    match e {
        IngestError::Rejected(rejection) => Some((rejection, rejection.job_index())),
        IngestError::ImportBusy
        | IngestError::HubRateLimited { .. }
        | IngestError::Upstream { .. }
        | IngestError::Other(_) => None,
    }
}

/// The catalogs a plan is checked against, loaded once per request, and the
/// vault resources it may name.
struct IngestCatalogs {
    benchmarks: HashMap<BenchmarkId, Benchmark>,
    /// `None` skips the uncatalogued-model warnings, which only filing reports.
    models: Option<ModelParamTable>,
    vault_scope: VaultScope,
}

impl IngestCatalogs {
    /// Load the benchmark catalog `scope` may name and the model parameter table, for
    /// filing ([`prepare`]).
    async fn load(state: &AppState, scope: &FilingScope) -> Result<Self, AppError> {
        let mut catalogs = Self::load_without_models(state, scope).await?;
        // Per request, not cached, so a fresh row does not warn as missing. Unreadable degrades
        // to empty: this table decides only a warning.
        let models = ModelParamTable::load(state.model_params_store.as_ref())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "model parameter table unreadable; ingesting this plan without coverage warnings"
                );
                ModelParamTable::empty()
            });
        catalogs.models = Some(models);
        Ok(catalogs)
    }

    /// Load the benchmark catalog `scope` may name, and no model parameter table, for a
    /// dry run ([`dry_run_plan`]) and `POST /api/v1/plans/validate`.
    async fn load_without_models(state: &AppState, scope: &FilingScope) -> Result<Self, AppError> {
        let public = state.catalog_cache.get().await.map_err(|e| {
            AppError::internal(
                e.context("failed to load the benchmark catalog for plan submission"),
            )
        })?;
        // A private id outside `scope` is refused as unknown, so the refusal
        // says nothing about another organization's catalog.
        let benchmarks =
            org_catalog::scoped_catalog(state.catalog_store.as_ref(), &public, &scope.catalog)
                .await
                .map_err(|e| {
                    AppError::internal(
                        e.context("failed to load the private benchmarks for plan submission"),
                    )
                })?;
        Ok(Self {
            benchmarks,
            models: None,
            vault_scope: scope.vault.clone(),
        })
    }

    /// The ingestion context over the server's stores and these catalogs. An
    /// import runs through `imports`.
    fn context<'a>(&'a self, state: &'a AppState, imports: &'a HfImports) -> IngestContext<'a> {
        IngestContext {
            plans: state.plan_store.as_ref(),
            todo: state.todo_store.as_ref(),
            clients: state.client_store.as_ref(),
            tags: state.tag_store.as_ref(),
            submissions: state.submission_store.as_ref(),
            orgs: state.org_store.as_ref(),
            vault: state.vault_store.as_ref(),
            vault_scope: &self.vault_scope,
            import: Some(imports),
            catalog: &self.benchmarks,
            // Warn, never reject: a model the table has not caught up with
            // must not cost a tenant their plan.
            models: self
                .models
                .as_ref()
                .map_or_else(ModelCheck::none, ModelCheck::warn),
            max_jobs: state.config.plan_max_jobs,
            creating_max_age: std::time::Duration::from_secs(
                state.config.plan_creating_max_age_secs.get(),
            ),
        }
    }
}

/// A plan that passed every check and its import, ready to file. [`prepare`]
/// returns the value, and [`file_prepared`] takes it.
pub(crate) struct PreparedPlan {
    note: Option<NonEmptyTrimmedString>,
    change: Option<pipette_plan_types::RevisionChange>,
    catalogs: IngestCatalogs,
    jobs: PreparedJobs,
    source: PlanSource,
    plan_warnings: Vec<String>,
}

impl PreparedPlan {
    /// File the plan as a campaign revision that `change` records.
    pub(crate) fn revising(self, change: pipette_plan_types::RevisionChange) -> Self {
        Self {
            change: Some(change),
            ..self
        }
    }
}

/// Check a plan and import each model that carries a token, for `owner`. This
/// writes vault records and nothing else.
pub(crate) async fn prepare(
    state: &AppState,
    filer: &UserId,
    scope: FilingScope,
    note: Option<NonEmptyTrimmedString>,
    owner: &VaultOwner,
    plan: ExpandedDocument,
    progress: Progress,
) -> Result<PreparedPlan, AppError> {
    check_note(note.as_ref())?;
    let ExpandedDocument {
        jobs,
        source,
        plan_warnings,
        benchmark_types,
    } = plan;
    let jobs = index_labels(jobs);
    let catalogs = IngestCatalogs::load(state, &scope).await?;
    // The manifest records the types this load gives, so they must be the ones
    // the document was expanded under.
    if let Some(id) = plan_document::retyped(&benchmark_types, &catalogs.benchmarks) {
        return Err(AppError::Conflict(format!(
            "benchmark {id} changed while the plan was filed; file it again"
        )));
    }

    // Recorded before anything is written: a dropped connection cancels this
    // handler silently, so this event with no `plan created` marks an
    // interrupted submission.
    let job_count = jobs.len();
    tracing::info!(
        user_id = %filer,
        job_count,
        "plan submission received"
    );

    let imports = state.hf_imports.clone().with_progress(progress);
    let jobs = prepare_jobs(&catalogs.context(state, &imports), owner, jobs)
        .await
        .inspect_err(|e| log_rejection(filer, e))?;
    Ok(PreparedPlan {
        note,
        change: None,
        catalogs,
        jobs,
        source,
        plan_warnings,
    })
}

/// Mint and stage a plan that [`prepare`] accepted ([`ingest_prepared`]).
/// Attribute the plan to `filer`, and file it under `membership`.
pub(crate) async fn file_prepared(
    state: &AppState,
    filer: &UserId,
    membership: CampaignMembership,
    prepared: PreparedPlan,
) -> Result<IngestReport, AppError> {
    let PreparedPlan {
        note,
        change,
        catalogs,
        jobs,
        source,
        plan_warnings,
    } = prepared;
    let submission = PlanSubmission {
        note,
        change,
        submitted_by: Some(filer.clone()),
        membership,
        jobs,
        source: Some(source),
        plan_warnings,
    };
    let report = ingest_prepared(&catalogs.context(state, &state.hf_imports), submission)
        .await
        .inspect_err(|e| log_rejection(filer, e))?;
    tracing::Span::current().record("plan_id", tracing::field::display(&report.plan_id));

    // The acceptance, tying the filer to the plan id the response carries;
    // `ingest_prepared` emits `plan created` itself.
    tracing::info!(
        user_id = %filer,
        plan_id = %report.plan_id,
        job_count = report.job_count,
        merged_job_count = report.merged_job_count,
        "plan submitted over HTTP",
    );
    Ok(report)
}

/// Log the caller's own rejection in `e`. A store failure becomes a `500`,
/// which `AppError::into_response` already records.
fn log_rejection(filer: &UserId, e: &IngestError) {
    if let Some((rejection, index)) = caller_rejection(e) {
        tracing::warn!(
            user_id = %filer,
            job_index = index,
            reason = %rejection,
            "rejected a submitted plan"
        );
    }
}

#[cfg(test)]
mod tests {
    use pipette_plan_types::{JobId, Warning, WarningKind};

    use super::*;

    /// The `201` body rewrites a fleet warning for a caller that may not read
    /// the fleet, and passes uncatalogued-model and planner warnings through.
    #[test]
    fn report_withholds_only_the_fleet_warning() {
        let model_message = "1 job names a model with no parameter-count entry".to_string();
        let planner_message = "variant 0: `exclude_clients` denies every client".to_string();
        let report = pipette_pipeline::plan_ingestion::IngestReport {
            plan_id: pipette_plan_types::PlanId::from_uuid(uuid::Uuid::nil()),
            note: None,
            change: None,
            job_count: 2,
            merged_job_count: 0,
            job_ids: vec![],
            merges: vec![],
            warnings: vec![
                Warning {
                    kind: WarningKind::Fleet,
                    message: "1 job requiring os:secret matches no working client".into(),
                    job_ids: vec![JobId::new_unchecked("job-1")],
                },
                Warning {
                    kind: WarningKind::UncataloguedModel,
                    message: model_message.clone(),
                    job_ids: vec![JobId::new_unchecked("job-2")],
                },
            ],
            plan_warnings: vec![planner_message.clone()],
        };
        let projected = report.view(FleetDetail::Withheld);
        assert_eq!(
            projected
                .warnings
                .iter()
                .map(|w| w.message.as_str())
                .collect::<Vec<_>>(),
            vec!["1 job matches no working client", model_message.as_str()]
        );
        assert_eq!(projected.plan_warnings, vec![planner_message]);
    }
}
