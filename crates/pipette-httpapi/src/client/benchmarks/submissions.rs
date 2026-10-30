//! A device's results: `POST /benchmarks` and `POST /benchmarks/batch`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use uuid::Uuid;

use pipette_core::benchmark::Benchmark;
use pipette_core::client::{Client, ClientStatus};
use pipette_core::denial::DenialRecord;
use pipette_pipeline::submission;
use pipette_pipeline::submission::{RequestedFlags, Submission, SubmissionInputExt};
use pipette_plan_types::{
    AuditTarget, BatchItem, BatchSubmitted, BenchmarkDefinition, BenchmarkId, BenchmarkParams,
    ClientId, ExpiresAt, JobAccepted, JobId, OrgId, SubmissionInput,
};
use pipette_store::{RecycleResult, RenewLeaseResult, SubmissionStore, TodoStore};

use crate::audit::Audit;
use crate::auth::AuthenticatedClient;
use crate::client::benchmarks::org_benchmark;
use crate::error::AppError;
use crate::extract::{SubmissionJson, SubmissionReader};
use crate::state::AppState;

/// A submission's benchmark, and whether its client may read the definition.
pub(crate) struct ResolvedBenchmark {
    benchmark: Benchmark,
    /// `false` when reached only through the client's claim: the definition
    /// stays hidden, so no answer may depend on it.
    readable: bool,
}

/// Public entries, then the client's organization's, then — for a failure on
/// the client's own claim — the claimed job's benchmark wherever it lives, so
/// an outsider can hand back a private job it was never served.
pub(crate) struct SubmissionCatalog<'a> {
    public: std::sync::Arc<HashMap<BenchmarkId, Benchmark>>,
    client: &'a Client,
    /// Resolved on the first public miss, so a public id costs no owner read.
    org_id: tokio::sync::OnceCell<Option<OrgId>>,
}

impl<'a> SubmissionCatalog<'a> {
    pub(crate) async fn load(state: &AppState, client: &'a Client) -> Result<Self, AppError> {
        let public = state.catalog_cache.get().await.map_err(|e| {
            AppError::Internal(format!(
                "failed to load the benchmark catalog to validate a submission: {e:#}"
            ))
        })?;
        Ok(Self {
            public,
            client,
            org_id: tokio::sync::OnceCell::new(),
        })
    }

    async fn resolve(
        &self,
        state: &AppState,
        held: bool,
        body: &serde_json::Value,
    ) -> Result<ResolvedBenchmark, AppError> {
        let readable = |benchmark| ResolvedBenchmark {
            benchmark,
            readable: true,
        };
        let benchmark_id = BenchmarkId::try_new(
            body.get("benchmark_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AppError::BadRequest("missing benchmark_id".into()))?,
        )
        .map_err(|e| AppError::BadRequest(format!("invalid benchmark_id: {e}")))?;
        if let Some(benchmark) = self.public.get(&benchmark_id) {
            return Ok(readable(benchmark.clone()));
        }
        let org_id = self
            .org_id
            .get_or_try_init(|| crate::auth::client_catalog_org(state, self.client))
            .await?;
        if let Some(benchmark) = org_benchmark(state, org_id.clone(), &benchmark_id).await? {
            return Ok(readable(benchmark));
        }
        // A held client holds no claim, and only a failure reports one.
        let is_failure = body.get("message_type").and_then(|v| v.as_str()) == Some("failure");
        let claimed_job = body
            .get("job_id")
            .and_then(|v| v.as_str())
            .and_then(|s| JobId::try_new(s).ok())
            .filter(|_| is_failure && !held);
        if let Some(job_id) = claimed_job
            && let Some(benchmark) = self
                .claimed_benchmark(state, &job_id, &benchmark_id)
                .await?
        {
            return Ok(ResolvedBenchmark {
                benchmark,
                readable: false,
            });
        }
        Err(AppError::NotFound("benchmark not found".into()))
    }

    /// `None` unless the client's own leased job runs it, so an outsider without
    /// that claim learns nothing about the id.
    async fn claimed_benchmark(
        &self,
        state: &AppState,
        job_id: &JobId,
        benchmark_id: &BenchmarkId,
    ) -> Result<Option<Benchmark>, AppError> {
        let client_id = &self.client.client_id;
        let failed = |step: &str, e: anyhow::Error| {
            AppError::Internal(format!(
                "failed to {step} to resolve a claimed job's benchmark: {e:#}"
            ))
        };
        let todo = state.todo_store.as_ref();
        let Some((expires_at, lease_expiry)) = own_lease(todo, client_id, job_id)
            .await
            .map_err(|e| failed("list the client's leases", e))?
        else {
            return Ok(None);
        };
        let leased = leased_body_at_current_key(todo, job_id, expires_at, client_id, lease_expiry)
            .await
            .map_err(|e| failed("read the leased job body", e))?;
        let runs_it = leased
            .as_ref()
            .and_then(|(_, _, b)| b.get("spec")?.get("benchmark")?.as_str())
            == Some(benchmark_id.as_str());
        if !runs_it {
            return Ok(None);
        }
        pipette_pipeline::org_catalog::server_benchmark(
            state.catalog_store.as_ref(),
            &self.public,
            benchmark_id,
        )
        .await
        .map_err(|e| failed("read the organizations' catalogs", e))
    }
}

/// Logs the keys a declaration took from the catalog and those no field reads,
/// so the fill can be retired once no client relies on it.
#[expect(
    clippy::disallowed_methods,
    reason = "compares the declaration's keys with the body's for a log line; nothing is answered"
)]
fn log_declaration_gaps(
    body: &serde_json::Value,
    declared: &BenchmarkParams,
    benchmark_id: &BenchmarkId,
) {
    let (Some(sent), Ok(serde_json::Value::Object(read))) = (
        body.get("benchmark_params")
            .and_then(serde_json::Value::as_object),
        serde_json::to_value(declared),
    ) else {
        return;
    };
    let filled: Vec<&str> = read
        .keys()
        .filter(|k| *k != "benchmark_type" && !sent.contains_key(*k))
        .map(String::as_str)
        .collect();
    let ignored: Vec<&str> = sent
        .iter()
        .filter(|(k, v)| !v.is_null() && *k != "benchmark_type" && !read.contains_key(*k))
        .map(|(k, _)| k.as_str())
        .collect();
    if !filled.is_empty() || !ignored.is_empty() {
        tracing::info!(
            %benchmark_id,
            ?filled,
            ?ignored,
            "declared benchmark_params read over the catalog"
        );
    }
}

/// Whether the entry draws each sample's seed by a named rule, which a result
/// must declare ([`validate_and_prepare_submission`]).
fn names_seed_rule(def: &BenchmarkDefinition) -> bool {
    match def {
        BenchmarkDefinition::VlmEval(b) => b.parameter_seed_rule.is_some(),
        BenchmarkDefinition::AsrEval(b) => b.parameter_seed_rule.is_some(),
        BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_) => false,
    }
}

/// Also returns whether the client supplied the `job_id`: only such an id can
/// match a queued job, so it gates the claim binding and the `todo/` teardown.
fn validate_and_prepare_submission(
    body: serde_json::Value,
    client_id: &ClientId,
    resolved: &ResolvedBenchmark,
) -> Result<(Submission, bool), AppError> {
    let benchmark = &resolved.benchmark;
    // Peeked from the raw body: `job_id` is not an `*Input` field. Only planned
    // runs report failures, so a failure without one is a `400`, never a mint.
    let message_type = match body.get("message_type") {
        None => Some("success"),
        Some(tag) => tag.as_str(),
    };
    let is_failure = message_type == Some("failure");

    // Raw-body check: serde drops these keys before the typed value exists.
    if matches!(message_type, Some("success" | "failure")) {
        submission::reject_wrong_variant_fields(&body, is_failure)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
    }
    let client_supplied_job_id = matches!(body.get("job_id"), Some(v) if !v.is_null());
    let job_id = match body.get("job_id") {
        None | Some(serde_json::Value::Null) if is_failure => {
            return Err(AppError::BadRequest(
                "failure submission requires job_id".into(),
            ));
        }
        None | Some(serde_json::Value::Null) => JobId::from_uuid(Uuid::now_v7()),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| AppError::BadRequest("job_id must be a string".into()))?;
            // Not required to be a UUID: `verify_claim` binds it to a lease.
            JobId::try_new(s)?
        }
    };

    // Raw-body check: serde silently merges the aliases.
    if matches!(&benchmark.def, BenchmarkDefinition::MaxMemoryUsage(_)) {
        submission::reject_max_alias_collisions(&body)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
    }

    // A hidden definition is neither read over nor compared against: either
    // would reveal it, so its declaration is taken as if never sent.
    if resolved.readable {
        let catalog = benchmark.params();
        match BenchmarkParams::from_submission_body(&body, Some(&catalog))
            .map_err(|e| AppError::BadRequest(e.to_string()))?
        {
            Some(declared) => {
                log_declaration_gaps(&body, &declared, benchmark.benchmark_id());
                submission::check_declared_benchmark_params(&declared, &catalog)
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
            }
            // Only its declaration tells a result drawn by the entry's seed
            // rule from one drawn by the repeat offset, so a result that sends
            // none may come from a client that predates the rule: it is refused
            // rather than filed under the rule. A failure files no answers.
            None if !is_failure && names_seed_rule(&benchmark.def) => {
                return Err(AppError::BadRequest(format!(
                    "benchmark {} names a seed_rule: a result must declare its \
                     benchmark_type and benchmark_params",
                    benchmark.benchmark_id()
                )));
            }
            None => {}
        }
    }

    let input: SubmissionInput = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("invalid submission: {e}")))?;

    input
        .validate(benchmark)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    Ok((
        input.into_submission(
            client_id.clone(),
            job_id,
            Utc::now(),
            benchmark.benchmark_type(),
        ),
        client_supplied_job_id,
    ))
}

// POST /benchmarks (submit results)
pub async fn submit_benchmark(
    State(state): State<AppState>,
    Submitter {
        client,
        disposition,
    }: Submitter,
    audit: Audit,
    SubmissionJson(body, _turn): SubmissionJson,
) -> Result<impl IntoResponse, AppError> {
    let held = disposition.held();

    let catalog = SubmissionCatalog::load(&state, &client).await?;
    let resolved = catalog.resolve(&state, held, &body).await?;
    tracing::Span::current().record(
        "benchmark_id",
        tracing::field::display(resolved.benchmark.benchmark_id()),
    );
    let (mut submission, client_supplied_job_id) =
        validate_and_prepare_submission(body, &client.client_id, &resolved)?;

    let job_id = submission_job_id(&submission).clone();
    tracing::Span::current().record("job_id", tracing::field::display(&job_id));
    audit
        .involve(
            &state,
            [AuditTarget::Job {
                job_id: job_id.clone(),
            }],
        )
        .await;

    // Ahead of every write, the lease renewal included: a client that may not
    // report a claimed job must not renew a lease on one.
    disposition.authorize(client_supplied_job_id)?;

    // `incoming/` is not partitioned by client, so an unbound `job_id` could
    // clobber another client's record. Held and ad-hoc ids have no shared key.
    let requested_flags = if !held && client_supplied_job_id {
        Some(bind_claim(&state, &submission, &job_id, &client.client_id).await?)
    } else {
        None
    };

    if let Some(f) = retriable_failure(&submission, held, client_supplied_job_id) {
        handle_retriable_failure(
            &*state.todo_store,
            &*state.submission_store,
            f,
            &client.client_id,
            &job_id,
        )
        .await?;
        log_denied_retriable_failure(f);
        return Ok((
            StatusCode::ACCEPTED,
            Json(JobAccepted {
                job_id: job_id.to_string(),
            }),
        ));
    }

    if let Some(flags) = requested_flags {
        submission.set_requested_flags(flags);
    }

    record_submission(&*state.submission_store, &submission, held).await?;
    log_accepted_submission(&submission);

    if !held && client_supplied_job_id {
        teardown_completed_job(&*state.todo_store, &job_id, &client.client_id).await;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(JobAccepted {
            job_id: job_id.to_string(),
        }),
    ))
}

/// The status step, resolved once per request. The permission step is
/// [`Self::authorize`]: it depends on the body, which an extractor cannot read.
/// A held submission carries no rights, so nothing can consult them.
pub(crate) enum SubmissionDisposition {
    Pipeline(crate::auth::ClientRights),
    Held,
}

impl SubmissionDisposition {
    /// Rights are read here, once per request, however many batch items follow.
    async fn resolve(state: &AppState, client: &Client) -> Result<Self, AppError> {
        if client.status == ClientStatus::Approved {
            return Ok(Self::Pipeline(
                crate::auth::ClientRights::load(state, &client.client_id).await?,
            ));
        }
        if state.config.unverified_submissions.enabled {
            tracing::info!("holding submission from unapproved client in unverified archive");
            Ok(Self::Held)
        } else {
            tracing::warn!("submission rejected: client not approved");
            Err(AppError::Forbidden("client is not approved".into()))
        }
    }

    fn held(&self) -> bool {
        matches!(self, Self::Held)
    }

    /// A held submission needs no grant: grants follow vetting, and the hold is
    /// for clients nobody has vetted yet.
    fn authorize(&self, client_supplied_job_id: bool) -> Result<(), AppError> {
        match self {
            Self::Pipeline(rights) => rights.require(if client_supplied_job_id {
                pipette_plan_types::ClientPermission::PerformWork
            } else {
                pipette_plan_types::ClientPermission::SubmitResults
            }),
            Self::Held => Ok(()),
        }
    }
}

/// The submission routes' gate, settled from the headers, so a client refused
/// for its status never has its body read. Spends the request's signature: take one extractor, never two.
pub struct Submitter {
    client: Client,
    disposition: SubmissionDisposition,
}

impl FromRequestParts<AppState> for Submitter {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthenticatedClient(client) =
            AuthenticatedClient::from_request_parts(parts, state).await?;
        let disposition = SubmissionDisposition::resolve(state, &client).await?;
        // [`SubmissionJson`] reads one body at a time per device, a held one
        // within its own ceiling.
        parts.extensions.insert(SubmissionReader {
            client_id: client.client_id.clone(),
            held: disposition.held(),
        });
        Ok(Self {
            client,
            disposition,
        })
    }
}

async fn record_submission(
    store: &dyn SubmissionStore,
    submission: &Submission,
    held: bool,
) -> Result<(), AppError> {
    submission::record_submission(store, submission, held)
        .await
        .map_err(|e| AppError::Internal(format!("failed to write the submission record: {e:#}")))
}

/// A missing body warns and records no request.
fn requested_flags_from_body(body: Option<&serde_json::Value>) -> RequestedFlags {
    match body.map(|b| b.get("spec")) {
        Some(Some(spec)) => submission::requested_flags_from_spec(spec),
        Some(None) => {
            tracing::warn!("leased job body carries no `spec`; recording no requested flags");
            RequestedFlags::default()
        }
        None => {
            tracing::warn!("leased job body is gone; recording no requested flags");
            RequestedFlags::default()
        }
    }
}

/// The lease's key, which every renewal renames, and the job body read at it.
pub(crate) type LeasedBody = (ExpiresAt, DateTime<Utc>, serde_json::Value);

pub(crate) async fn own_leased_body(
    todo: &dyn TodoStore,
    client_id: &ClientId,
    job_id: &JobId,
) -> anyhow::Result<Option<LeasedBody>> {
    let Some((expires_at, lease_expiry)) = own_lease(todo, client_id, job_id).await? else {
        return Ok(None);
    };
    leased_body_at_current_key(todo, job_id, expires_at, client_id, lease_expiry).await
}

/// Every renewal renames a lease's key, so on a miss this re-locates the lease
/// once. A failed listing propagates: a NULL would read as "no flags" forever.
async fn leased_body_at_current_key(
    todo: &dyn TodoStore,
    job_id: &JobId,
    expires_at: ExpiresAt,
    client_id: &ClientId,
    lease_expiry: DateTime<Utc>,
) -> anyhow::Result<Option<LeasedBody>> {
    if let Some(body) = todo
        .get_leased(job_id, expires_at, client_id, lease_expiry)
        .await?
    {
        return Ok(Some((expires_at, lease_expiry, body)));
    }
    let Some((current_expires_at, current_lease_expiry)) =
        own_lease(todo, client_id, job_id).await?
    else {
        return Ok(None);
    };
    tracing::info!("lease renewed since it was located; reading the job body at its current key");
    Ok(todo
        .get_leased(job_id, current_expires_at, client_id, current_lease_expiry)
        .await?
        .map(|body| (current_expires_at, current_lease_expiry, body)))
}

/// The job body is read before [`verify_claim`] renews the lease, so a refused
/// success renews nothing. Returns the flags the job asks for.
async fn bind_claim(
    state: &AppState,
    submission: &Submission,
    job_id: &JobId,
    client_id: &ClientId,
) -> Result<RequestedFlags, AppError> {
    let todo = &*state.todo_store;
    let success = matches!(submission, Submission::Success(_));
    let body = match own_leased_body(todo, client_id, job_id).await {
        Ok(leased) => leased.map(|(_, _, body)| body),
        Err(e) if success => {
            return Err(AppError::Internal(format!(
                "failed to read the leased job body to check its vault resources: {e:#}"
            )));
        }
        // A failure needs no vault check, and a `todo/` hiccup must not fail it.
        Err(e) => {
            tracing::warn!(error = %e, "failed to read the leased job body; recording no requested flags");
            None
        }
    };
    if success && let Some(body) = &body {
        require_fetchable_vault(state, body, client_id).await?;
    }
    verify_claim(
        todo,
        job_id,
        client_id,
        state.config.lease_expiry_from(Utc::now()),
    )
    .await?;
    // A lease that appeared after the read skipped the vault check.
    if success && body.is_none() {
        return Err(AppError::NotFound(
            "no active claim; reclaim before submitting".into(),
        ));
    }
    Ok(requested_flags_from_body(body.as_ref()))
}

/// A device refused a job's vault weights never ran them, so its measured
/// result is refused; its failure, which needs no weights, is not.
async fn require_fetchable_vault(
    state: &AppState,
    body: &serde_json::Value,
    client_id: &ClientId,
) -> Result<(), AppError> {
    if crate::vault_access::client_may_fetch_job(state, client_id, body).await? {
        return Ok(());
    }
    tracing::warn!(
        reason = "vault_out_of_reach",
        "claim verification rejected a success: the job names a vault resource the client may not fetch"
    );
    Err(AppError::Forbidden(
        "the job names a vault resource this client may not fetch; only a failure may be reported"
            .into(),
    ))
}

/// Authorizes by renewing the lease, not reading it, so `queue-maintenance`
/// cannot recycle it while the result write lands; an expired but unrecycled
/// lease still passes. A pending reindex refuses before the renewal could
/// resurrect a relinquished lease.
async fn verify_claim(
    todo: &dyn TodoStore,
    job_id: &JobId,
    client_id: &ClientId,
    new_expiry: DateTime<Utc>,
) -> Result<ExpiresAt, AppError> {
    if todo.has_pending_reindex(client_id).await.map_err(|e| {
        AppError::Internal(format!(
            "claim verification failed to read the pending-reindex flag: {e:#}"
        ))
    })? {
        tracing::warn!(
            reason = "pending_reindex",
            "refused submission during pending reindex — the flag voids the client's claim; the result is forfeited"
        );
        return Err(AppError::NotFound(
            "no active claim; client re-evaluation pending".into(),
        ));
    }
    match todo
        .renew_lease(job_id, client_id, new_expiry)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "claim verification failed to renew the lease: {e:#}"
            ))
        })? {
        RenewLeaseResult::Renewed(expires_at) => Ok(expires_at),
        RenewLeaseResult::WrongClient => {
            tracing::warn!(
                reason = "leased_elsewhere",
                "claim verification rejected submission"
            );
            Err(AppError::Conflict(
                "the job is leased to another client".into(),
            ))
        }
        RenewLeaseResult::NotFound => {
            tracing::warn!(
                reason = "no_active_claim",
                "claim verification rejected submission"
            );
            Err(AppError::NotFound(
                "no active claim; reclaim before submitting".into(),
            ))
        }
    }
}

/// Terminal only: a retriable failure keeps the job for other clients. Best
/// effort; `queue-maintenance` reconciles what is left, `denied/` and
/// `eligible/` included.
async fn teardown_completed_job(todo: &dyn TodoStore, job_id: &JobId, client_id: &ClientId) {
    let drop_lease = async {
        match own_lease(todo, client_id, job_id).await {
            Ok(Some((expires_at, expiry))) => {
                if let Err(e) = todo
                    .delete_lease(job_id, expires_at, client_id, expiry)
                    .await
                {
                    tracing::warn!(error = %e, "todo teardown: delete_lease failed");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "todo teardown: list_leased_for_client failed");
            }
        }
    };
    let drop_avail = async {
        if let Err(e) = todo.delete_avail_by_job(job_id).await {
            tracing::warn!(error = %e, "todo teardown: delete_avail_by_job failed");
        }
    };
    tokio::join!(drop_lease, drop_avail);
}

fn retriable_failure(
    submission: &Submission,
    held: bool,
    client_supplied_job_id: bool,
) -> Option<&pipette_pipeline::submission::FailureSubmission> {
    if held || !client_supplied_job_id {
        return None;
    }
    match submission {
        Submission::Failure(f) if f.wire.retriable => Some(f.as_ref()),
        Submission::Failure(_) | Submission::Success(_) => None,
    }
}

/// A failure to list propagates rather than reading as "no lease".
async fn own_lease(
    todo: &dyn TodoStore,
    client_id: &ClientId,
    job_id: &JobId,
) -> anyhow::Result<Option<(ExpiresAt, chrono::DateTime<Utc>)>> {
    Ok(todo
        .list_leased_for_client(client_id)
        .await?
        .iter()
        .find_map(|k| {
            let lease = pipette_core::todo_filename::parse_leased_key(k).ok()?;
            (lease.job_id == *job_id).then_some((lease.expires_at, lease.lease_expiry))
        }))
}

/// No `denied/` marker: the client may still match under its new profile. A
/// renewal past its flag check can still rename a lease, so the loop re-lists
/// until the partition is clean, a bounded number of times.
pub(crate) async fn relinquish_client_leases(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    client_id: &ClientId,
) -> Result<(), AppError> {
    // Unparseable keys are foreign cruft and must not fail the clean check.
    async fn parsed_leases(
        todo: &dyn TodoStore,
        client_id: &ClientId,
    ) -> Result<Vec<pipette_core::todo_filename::Lease>, AppError> {
        Ok(todo
            .list_leased_for_client(client_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!("failed to list the leases to relinquish: {e:#}"))
            })?
            .into_iter()
            .filter_map(|key| match pipette_core::todo_filename::parse_leased_key(&key) {
                Ok(parsed) => Some(parsed),
                Err(_) => {
                    tracing::warn!(key = %key, "skipping unparseable leased key during relinquish");
                    None
                }
            })
            .collect())
    }

    const RELINQUISH_PASSES: usize = 2;
    // The last pass only verifies.
    for pass in 0..=RELINQUISH_PASSES {
        let leases = parsed_leases(todo, client_id).await?;
        if leases.is_empty() {
            return Ok(());
        }
        if pass == RELINQUISH_PASSES {
            break;
        }
        if pass > 0 {
            tracing::warn!(
                remaining = leases.len(),
                "protocol violation: lease renewed mid-relinquish — a broken client is renewing while its own profile update relinquishes; re-resolving"
            );
        }
        for lease in leases {
            let job_id = lease.job_id;
            match pipette_pipeline::queue_maintenance::resolve_or_recycle_lease(
                todo,
                submissions,
                &job_id,
                lease.expires_at,
                &lease.client_id,
                lease.lease_expiry,
            )
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "failed to relinquish the lease on job {job_id}: {e:#}"
                ))
            })? {
                pipette_pipeline::queue_maintenance::LeaseResolution::Recycled => {
                    if !delete_recycled_entry_if_recorded(todo, submissions, &job_id).await? {
                        tracing::info!(
                            job_id = %job_id,
                            "profile change: relinquished lease; job returned to avail/"
                        );
                    }
                }
                pipette_pipeline::queue_maintenance::LeaseResolution::ResolvedStale => {
                    tracing::info!(
                        job_id = %job_id,
                        "profile change: job already has a submission record; deleted stale lease"
                    );
                }
                pipette_pipeline::queue_maintenance::LeaseResolution::Gone => {
                    // Either another actor resolved it (fine) or an in-flight
                    // renewal renamed it — the next re-list decides.
                    tracing::debug!(job_id = %job_id, "lease already resolved elsewhere during relinquish");
                }
            }
        }
    }
    Err(AppError::Internal(format!(
        "the client still holds leases after {RELINQUISH_PASSES} relinquish passes; \
         failing profile update"
    )))
}

/// A result write racing the relinquish can land its record mid-recycle,
/// leaving a job both recorded and claimable that no maintenance pass repairs.
async fn delete_recycled_entry_if_recorded(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    job_id: &JobId,
) -> Result<bool, AppError> {
    if submissions
        .find_job(job_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to re-check the record for job {job_id} after recycle: {e:#}"
            ))
        })?
        .is_none()
    {
        return Ok(false);
    }
    todo.delete_avail_by_job(job_id).await.map_err(|e| {
        AppError::Internal(format!(
            "failed to remove the recycled avail entry for job {job_id}: {e:#}"
        ))
    })?;
    tracing::warn!(
        job_id = %job_id,
        "relinquish raced an in-flight submission: record landed during recycle; removed the recycled avail/ entry"
    );
    Ok(true)
}

/// The recycle is the point of no return: it destroys the lease a retry needs,
/// so every fallible step runs ahead of it, and each is idempotent.
async fn handle_retriable_failure(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    failure: &pipette_pipeline::submission::FailureSubmission,
    client_id: &ClientId,
    job_id: &JobId,
) -> Result<(), AppError> {
    submission::write_runtime_output(submissions, job_id, client_id, &failure.wire.runtime_output)
        .await
        .map_err(|e| failed_step("write the runtime output", e))?;

    // The denial is durable before the recycle, and unconditional: without it
    // `claim` could hand the job straight back to this client.
    let record = DenialRecord::new(
        failure.submitted_at,
        failure.wire.failure_reason.as_str(),
        failure.wire.client_version.as_ref().map(|v| v.as_str()),
        failure.wire.runtime_version.as_ref().map(|v| v.as_str()),
    );
    let (denied, leased) = tokio::join!(
        todo.write_denied(job_id, client_id, &record),
        own_lease(todo, client_id, job_id),
    );
    denied.map_err(|e| failed_step("write the denied marker", e))?;
    let Some((expires_at, lease_expiry)) =
        leased.map_err(|e| failed_step("list the client's leases", e))?
    else {
        return Ok(());
    };

    let job_body = todo
        .get_leased(job_id, expires_at, client_id, lease_expiry)
        .await
        .map_err(|e| failed_step("read the job body", e))?;

    // Only a signal to log: `verify_claim` already authorized the caller.
    if let Some(body) = &job_body
        && let Some((reported, expected)) = benchmark_mismatch(failure, body)
    {
        tracing::warn!(
            reported,
            expected = expected.unwrap_or("<absent>"),
            "retriable failure report names a different benchmark than the claimed job"
        );
    }

    // Recycle, not `delete_lease`: the job body lives only under `leased/`
    // while claimed. `Gone` means the client raced itself: `verify_claim` just
    // renewed the lease. The denial stands, and `queue-maintenance` reconciles
    // the rest.
    if let RecycleResult::Gone = todo
        .recycle_lease(job_id, expires_at, client_id, lease_expiry)
        .await
        .map_err(|e| failed_step("recycle the lease", e))?
    {
        tracing::warn!(
            "lease vanished mid-recycle: client raced its own failure submission; left to queue-maintenance reconciliation"
        );
    }
    Ok(())
}

fn failed_step(step: &str, e: anyhow::Error) -> AppError {
    AppError::Internal(format!(
        "retriable failure submission: failed to {step}: {e:#}"
    ))
}

/// Only `benchmark_id`: a failure report no longer echoes `model_*`/`runtime_*`.
/// An unreadable `spec.benchmark` counts as a disagreement.
fn benchmark_mismatch<'a>(
    failure: &'a pipette_pipeline::submission::FailureSubmission,
    job_body: &'a serde_json::Value,
) -> Option<(&'a str, Option<&'a str>)> {
    let reported = failure.wire.benchmark_id.as_str();
    let expected = job_body
        .get("spec")
        .and_then(|spec| spec.get("benchmark"))
        .and_then(|v| v.as_str());
    (expected != Some(reported)).then_some((reported, expected))
}

fn submission_job_id(s: &Submission) -> &JobId {
    match s {
        Submission::Success(s) => &s.job_id,
        Submission::Failure(f) => &f.job_id,
    }
}

fn submission_benchmark_id(s: &Submission) -> &BenchmarkId {
    match s {
        Submission::Success(s) => &s.wire.benchmark_id,
        Submission::Failure(f) => &f.wire.benchmark_id,
    }
}

/// The one record of a retriable failure; same rules as
/// [`log_accepted_submission`].
fn log_denied_retriable_failure(failure: &pipette_pipeline::submission::FailureSubmission) {
    tracing::warn!(
        benchmark_id = %failure.wire.benchmark_id,
        benchmark_type = %failure.benchmark_type,
        model_name = ?failure.wire.model_name,
        model_quant = ?failure.wire.model_quant,
        runtime_name = ?failure.wire.runtime_name,
        client_version = ?failure.wire.client_version.as_ref().map(|v| v.as_str()),
        runtime_version = ?failure.wire.runtime_version.as_ref().map(|v| v.as_str()),
        failure_reason = ?failure.wire.failure_reason.as_str(),
        "job denied for client: retriable failure"
    );
}

/// Call only once the result is durable. Client free text uses `Debug`, so a
/// raw newline cannot forge a log record of its own.
fn log_accepted_submission(submission: &Submission) {
    match submission {
        Submission::Success(s) => {
            tracing::info!(
                benchmark_id = %s.wire.benchmark_id,
                benchmark_type = %s.benchmark_type,
                model_name = ?s.wire.model_name,
                model_quant = ?s.wire.model_quant,
                runtime_name = ?s.wire.runtime_name,
                "accepted submission"
            );
        }
        Submission::Failure(f) => {
            tracing::info!(
                benchmark_id = %f.wire.benchmark_id,
                benchmark_type = %f.benchmark_type,
                model_name = ?f.wire.model_name,
                model_quant = ?f.wire.model_quant,
                runtime_name = ?f.wire.runtime_name,
                retriable = f.wire.retriable,
                failure_reason = ?f.wire.failure_reason.as_str(),
                "accepted failure submission"
            );
        }
    }
}

const BATCH_MAX_SUBMISSIONS: usize = 1000;

const BATCH_CONCURRENCY: usize = 16;

/// A rejected item never passes through `AppError`'s `IntoResponse`, so it is
/// logged here in full.
fn reject_batch_item(index: usize, e: &AppError) -> BatchItem {
    // A `200` body hides whose fault the item was, so the level carries it.
    if e.is_service_fault() {
        tracing::error!(error = %e, "failed to process batch submission");
    } else {
        tracing::warn!(error = %e, "rejected batch submission");
    }
    BatchItem::Rejected {
        index,
        error: e.message().to_string(),
    }
}

/// Errors become the item's entry, so one bad item doesn't fail the batch.
#[tracing::instrument(
    skip_all,
    fields(batch_index = index, job_id = tracing::field::Empty)
)]
async fn process_batch_item(
    state: &AppState,
    client_id: &ClientId,
    catalog: &SubmissionCatalog<'_>,
    disposition: &SubmissionDisposition,
    index: usize,
    item: serde_json::Value,
) -> BatchItem {
    let held = disposition.held();
    let resolved = match catalog.resolve(state, held, &item).await {
        Ok(resolved) => resolved,
        Err(e) => return reject_batch_item(index, &e),
    };
    let (mut submission, client_supplied_job_id) =
        match validate_and_prepare_submission(item, client_id, &resolved) {
            Ok(s) => s,
            Err(e) => return reject_batch_item(index, &e),
        };

    let job_id = submission_job_id(&submission).clone();
    let benchmark_id = submission_benchmark_id(&submission).clone();
    tracing::Span::current().record("job_id", tracing::field::display(&job_id));

    // Per item: each item names its own operation.
    if let Err(e) = disposition.authorize(client_supplied_job_id) {
        return reject_batch_item(index, &e);
    }

    let requested_flags = if !held && client_supplied_job_id {
        match bind_claim(state, &submission, &job_id, client_id).await {
            Ok(flags) => Some(flags),
            Err(e) => return reject_batch_item(index, &e),
        }
    } else {
        None
    };

    if let Some(f) = retriable_failure(&submission, held, client_supplied_job_id) {
        return match handle_retriable_failure(
            &*state.todo_store,
            &*state.submission_store,
            f,
            client_id,
            &job_id,
        )
        .await
        {
            Ok(()) => {
                log_denied_retriable_failure(f);
                BatchItem::Accepted {
                    index,
                    job_id: job_id.to_string(),
                }
            }
            Err(e) => reject_batch_item(index, &e),
        };
    }

    if let Some(flags) = requested_flags {
        submission.set_requested_flags(flags);
    }

    match record_submission(&*state.submission_store, &submission, held).await {
        Ok(()) => {
            tracing::info!(benchmark_id = %benchmark_id, "accepted batch submission");
            if !held && client_supplied_job_id {
                teardown_completed_job(&*state.todo_store, &job_id, client_id).await;
            }
            BatchItem::Accepted {
                index,
                job_id: job_id.to_string(),
            }
        }
        Err(e) => reject_batch_item(index, &e),
    }
}

// POST /benchmarks/batch
pub async fn submit_benchmark_batch(
    State(state): State<AppState>,
    Submitter {
        client,
        disposition,
    }: Submitter,
    audit: Audit,
    SubmissionJson(body, _turn): SubmissionJson,
) -> Result<impl IntoResponse, AppError> {
    let catalog = SubmissionCatalog::load(&state, &client).await?;
    let client_id = &client.client_id;

    let submissions = body
        .get("submissions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AppError::BadRequest("missing submissions array".into()))?;

    if submissions.is_empty() {
        return Err(AppError::BadRequest("submissions array is empty".into()));
    }
    if submissions.len() > BATCH_MAX_SUBMISSIONS {
        return Err(AppError::BadRequest(format!(
            "too many submissions (max {BATCH_MAX_SUBMISSIONS})"
        )));
    }

    let results: Vec<BatchItem> = futures::stream::iter(submissions.iter().cloned().enumerate())
        .map(|(i, item)| process_batch_item(&state, client_id, &catalog, &disposition, i, item))
        .buffered(BATCH_CONCURRENCY)
        .collect()
        .await;

    let accepted = results.iter().filter_map(|item| match item {
        BatchItem::Accepted { job_id, .. } => JobId::try_new(job_id.as_str()).ok(),
        BatchItem::Rejected { .. } => None,
    });
    audit
        .involve(&state, accepted.map(|job_id| AuditTarget::Job { job_id }))
        .await;
    Ok(Json(BatchSubmitted { results }))
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use pipette_plan_types::BenchmarkType;
    use pipette_store_fs::{LocalFsSubmissionStore, LocalFsTodoStore};

    use super::*;

    fn todo_store(root: &std::path::Path) -> anyhow::Result<LocalFsTodoStore> {
        let todo_dir = root.join("todo");
        [
            "avail",
            "leased",
            "denied",
            "eligible/clients",
            "pending-reindex",
            "tmp",
            "suspended",
        ]
        .iter()
        .try_for_each(|sub| std::fs::create_dir_all(todo_dir.join(sub)))?;
        Ok(LocalFsTodoStore::new(todo_dir))
    }

    fn failure_from(
        job_body: &serde_json::Value,
    ) -> anyhow::Result<Box<pipette_pipeline::submission::FailureSubmission>> {
        match pipette_pipeline::submission::system_failure_from_job_body(
            job_body,
            BenchmarkType::PrefillThroughput,
            "x",
            pipette_plan_types::FailureSource::Client,
            Utc::now(),
        )? {
            Submission::Failure(f) => Ok(f),
            Submission::Success(_) => {
                anyhow::bail!("system_failure_from_job_body returned a success")
            }
        }
    }

    /// The denial is still recorded, and nothing is synthesized into `avail/`.
    #[tokio::test]
    async fn retriable_failure_returns_ok_when_lease_vanished() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;

        let client = ClientId::try_new("c1")?;
        let job = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");
        let job_body = json!({
            "job_id": job.as_str(),
            "spec": {"benchmark": "prefill_throughput_256"},
        });
        let failure = failure_from(&job_body)?;

        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        handle_retriable_failure(&todo, &submissions, failure.as_ref(), &client, &job).await?;

        assert!(todo.is_denied(&job, &client).await?);
        assert!(todo.get_avail_by_job(&job).await?.is_none());
        Ok(())
    }

    /// `local_fs` parses a leased body as it reads it, so a body that is not
    /// JSON drives the read failure.
    #[rstest]
    #[case::readable_body_recycles(r#"{"spec":{"benchmark":"prefill_throughput_256"}}"#, true)]
    #[case::unreadable_body_keeps_the_lease("{not json", false)]
    #[tokio::test]
    async fn retriable_failure_keeps_the_lease_when_the_body_read_fails(
        #[case] planted_body: &str,
        #[case] handled: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;
        let client = ClientId::try_new("ev1_me")?;
        let job = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");

        let leased =
            dir.path()
                .join("todo")
                .join("leased")
                .join(pipette_core::todo_filename::leased_key(
                    &job,
                    ExpiresAt::Never,
                    &client,
                    Utc::now() + chrono::Duration::hours(1),
                ));
        std::fs::create_dir_all(leased.parent().context("lease path has no parent")?)?;
        std::fs::write(&leased, planted_body)?;

        let failure = failure_from(&json!({
            "job_id": job.as_str(),
            "spec": {"benchmark": "prefill_throughput_256"},
        }))?;
        assert_eq!(
            handle_retriable_failure(
                &todo,
                &LocalFsSubmissionStore::new(dir.path().join("submissions")),
                failure.as_ref(),
                &client,
                &job
            )
            .await
            .is_ok(),
            handled,
            "an unreadable body must fail the request, a readable one must not"
        );

        assert!(
            todo.is_denied(&job, &client).await?,
            "the denial must be durable whatever the read did"
        );
        assert_eq!(
            todo.get_avail_by_job(&job).await?.is_some(),
            handled,
            "the job belongs in avail/ only once the handling completed"
        );
        assert_eq!(
            todo.list_leased_for_client(&client).await?.is_empty(),
            handled,
            "the lease must survive a failed read so the retry can renew it"
        );
        Ok(())
    }

    #[rstest]
    #[case::stale_locator_follows_the_renewal(true, true, Some(r#"{"threads":8}"#))]
    #[case::current_locator_reads_directly(false, true, Some(r#"{"threads":8}"#))]
    #[case::no_lease_records_nothing(false, false, None)]
    #[tokio::test]
    async fn requested_flags_follow_a_renewed_lease(
        #[case] renew_after_locator: bool,
        #[case] plant_lease: bool,
        #[case] expected_runtime: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;
        let client = ClientId::try_new("ev1_me")?;
        let job = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");
        let locator_expiry = Utc::now() + chrono::Duration::hours(1);

        if plant_lease {
            let body = json!({"spec": {"runtime_flags": {"threads": 8}}});
            let leased = dir.path().join("todo").join("leased").join(
                pipette_core::todo_filename::leased_key(
                    &job,
                    ExpiresAt::Never,
                    &client,
                    locator_expiry,
                ),
            );
            std::fs::create_dir_all(leased.parent().context("lease path has no parent")?)?;
            std::fs::write(&leased, serde_json::to_vec(&body)?)?;
        }

        if renew_after_locator {
            let renewed = todo
                .renew_lease(&job, &client, locator_expiry + chrono::Duration::hours(1))
                .await?;
            assert!(
                matches!(renewed, RenewLeaseResult::Renewed(_)),
                "the planted lease should renew"
            );
        }

        let flags = requested_flags_from_body(
            leased_body_at_current_key(&todo, &job, ExpiresAt::Never, &client, locator_expiry)
                .await?
                .map(|(_, _, body)| body)
                .as_ref(),
        );

        assert_eq!(flags.runtime.as_deref(), expected_runtime);
        assert_eq!(flags.model, None);
        assert_eq!(flags.benchmark, None);
        Ok(())
    }

    #[tokio::test]
    async fn verify_claim_renews_expired_but_present_lease() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;
        let client = ClientId::try_new("ev1_me")?;
        let job = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");

        let old_expiry = Utc::now() - chrono::Duration::hours(2);
        let leased =
            dir.path()
                .join("todo")
                .join("leased")
                .join(pipette_core::todo_filename::leased_key(
                    &job,
                    ExpiresAt::Never,
                    &client,
                    old_expiry,
                ));
        std::fs::create_dir_all(leased.parent().context("lease path has no parent")?)?;
        std::fs::write(&leased, b"{}")?;

        let new_expiry = Utc::now() + chrono::Duration::hours(1);
        verify_claim(&todo, &job, &client, new_expiry).await?;

        let expiry = todo
            .list_leased_for_client(&client)
            .await?
            .iter()
            .find_map(|k| {
                let lease = pipette_core::todo_filename::parse_leased_key(k).ok()?;
                (lease.job_id == job).then_some(lease.lease_expiry)
            })
            .context("lease should still exist after renewal")?;
        assert!(
            expiry > Utc::now(),
            "lease expiry should be renewed into the future"
        );
        Ok(())
    }

    #[rstest]
    #[case::record_landed_removes_entry(true)]
    #[case::no_record_keeps_entry(false)]
    #[tokio::test]
    async fn recycled_entry_removed_only_when_recorded(
        #[case] recorded: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = todo_store(dir.path())?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let job = JobId::new_unchecked("job-recycled");

        let avail = dir.path().join("todo").join("avail").join(
            pipette_core::todo_filename::avail_filename(&job, pipette_plan_types::ExpiresAt::Never),
        );
        std::fs::create_dir_all(avail.parent().context("avail entry has a job directory")?)?;
        std::fs::write(
            &avail,
            serde_json::to_vec(&json!({ "job_id": "job-recycled" }))?,
        )?;
        if recorded {
            submissions
                .write_processed(
                    &job,
                    &json!({ "job_id": "job-recycled", "message_type": "success" }),
                )
                .await?;
        }

        let removed = delete_recycled_entry_if_recorded(&todo, &submissions, &job).await?;

        assert_eq!(removed, recorded);
        assert_eq!(todo.get_avail_by_job(&job).await?.is_none(), recorded);
        Ok(())
    }

    #[rstest]
    #[case::matches(json!({"spec": {"benchmark": "prefill_throughput_256"}}), None)]
    #[case::different_benchmark(
        json!({"spec": {"benchmark": "eval_test"}}),
        Some(Some("eval_test"))
    )]
    #[case::no_spec(json!({}), Some(None))]
    #[case::spec_without_benchmark(json!({"spec": {}}), Some(None))]
    #[case::non_string_benchmark(json!({"spec": {"benchmark": 42}}), Some(None))]
    fn benchmark_mismatch_flags_a_disagreeing_or_unreadable_benchmark(
        #[case] job_body: serde_json::Value,
        #[case] expected_side: Option<Option<&str>>,
    ) -> anyhow::Result<()> {
        let failure = failure_from(&json!({
            "job_id": "j",
            "spec": {"benchmark": "prefill_throughput_256"},
        }))?;

        let got = benchmark_mismatch(failure.as_ref(), &job_body);
        match expected_side {
            None => assert_eq!(got, None, "expected no mismatch"),
            Some(expected) => assert_eq!(got, Some(("prefill_throughput_256", expected))),
        }
        Ok(())
    }

    fn capture_logs(f: impl FnOnce()) -> anyhow::Result<String> {
        #[derive(Clone)]
        struct Writer(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                // A poisoned buffer means another thread panicked holding it;
                // report it as a write error rather than unwinding here.
                self.0
                    .lock()
                    .map_err(|_| std::io::Error::other("log capture buffer poisoned"))?
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        // Never the only registered dispatcher: alone, tracing-core caches a callsite's
        // interest from the first thread to reach it, often a sibling test with none.
        static SECOND: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
        SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Writer(Arc::clone(&buf)))
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let captured = buf
            .lock()
            .map_err(|e| anyhow::anyhow!("log capture buffer poisoned: {e}"))?;
        Ok(String::from_utf8_lossy(&captured).into_owned())
    }

    /// Asserted on the rendered line, so a future `Display` field fails here too.
    #[rstest]
    #[case::newline("boom\n2026-01-01T00:00:00.000000Z ERROR pipette_mgmt::auth: forged")]
    #[case::carriage_return("boom\rforged")]
    #[case::ansi_escape("boom\u{1b}[2K\u{1b}[31mforged")]
    fn accepted_failure_log_escapes_client_free_text(
        #[case] failure_reason: &str,
    ) -> anyhow::Result<()> {
        let submission: Submission = serde_json::from_value(json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": failure_reason,
            "client_id": "ev1_me",
            "job_id": "job-1",
            "submitted_at": "2026-03-10T12:00:00Z",
            "benchmark_type": "prefill_throughput",
        }))?;

        let captured = capture_logs(|| log_accepted_submission(&submission))?;
        let line = captured.strip_suffix('\n').unwrap_or(&captured);

        assert!(
            !line.contains(char::is_control),
            "no control character may reach the log line: {line:?}"
        );
        assert!(line.contains("boom"), "failure_reason is missing: {line:?}");
        Ok(())
    }
}
