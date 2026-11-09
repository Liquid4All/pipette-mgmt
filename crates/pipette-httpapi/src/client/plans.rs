//! The work queue a device drives: `POST /plans/claim`,
//! `PUT /plans/{job_id}/heartbeat` and `POST /plans/{job_id}/reclaim`.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};

use pipette_core::client::Client;
use pipette_core::denial::DenialRecord;
use pipette_core::matching::admitted_client_versions;
use pipette_core::todo_filename::Lease;
use pipette_pipeline::org_catalog::server_catalog;
use pipette_pipeline::queue_maintenance::{client_tags, settle_expired_lease};
use pipette_plan_types::{
    AuditTarget, ClaimAnswer, ClaimedJob, ClientId, ExpiresAt, FailureCode, JobId, UndecodableClaim,
};
use pipette_store::{ClaimOutcome, ReclaimOutcome, RenewLeaseResult, SubmissionStore, TodoStore};

use crate::audit::Audit;
use crate::auth::WorkingClient;
use crate::client::benchmarks::submissions::own_leased_body;
use crate::error::AppError;
use crate::extract::{ApiPath, Parsed};
use crate::state::AppState;
use crate::vault_access::client_may_fetch_job;

const VAULT_OUT_OF_REACH: &str = "the job names a vault resource this client may not fetch";

/// The gate and the claim are not atomic, so a PATCH's relinquish can miss a
/// lease created after its listing. Flag visible here: this side reverts it;
/// not visible: the relinquish lists it. Exactly one party cleans up.
async fn revert_claim_if_void(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    lease: &Lease,
) -> Result<bool, AppError> {
    if !todo
        .has_pending_reindex(&lease.client_id)
        .await
        .map_err(|e| {
            AppError::internal(
                e.context("failed to re-check the pending-reindex flag after granting the job"),
            )
        })?
    {
        return Ok(false);
    }
    // Every resolution counts as reverted, Gone included: a racing relinquish
    // resolved it.
    let resolution = pipette_pipeline::queue_maintenance::resolve_or_recycle_lease(
        todo,
        submissions,
        &lease.job_id,
        lease.expires_at,
        &lease.client_id,
        lease.lease_expiry,
    )
    .await
    .map_err(|e| AppError::internal(e.context("failed to revert the claim")))?;
    tracing::warn!(
        client_id = %lease.client_id,
        job_id = %lease.job_id,
        resolution = ?resolution,
        "lease reverted: the pending-reindex flag rose after the gate"
    );
    Ok(true)
}

/// Settles the client's lapsed leases with queue-maintenance's own step, so
/// that a heartbeat cannot revive one beside a new grant. A lease on a job with
/// a record is deleted. Any other lease counts as lost, and its job is recycled,
/// or retired once its losses reach the limit. `except` names the job that a
/// reclaim renews.
async fn settle_lapsed_leases(
    state: &AppState,
    client_id: &ClientId,
    except: Option<&JobId>,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    let todo = state.todo_store.as_ref();
    let leases = todo
        .leases_for_client(client_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the client's leases")))?;
    // A listing can surface both keys of a renewal rename; the job is then live.
    let live: HashSet<&JobId> = leases
        .iter()
        .filter(|lease| lease.lease_expiry > now)
        .map(|lease| &lease.job_id)
        .collect();
    let lapsed: Vec<&Lease> = leases
        .iter()
        .filter(|lease| !live.contains(&lease.job_id) && Some(&lease.job_id) != except)
        .collect();
    if lapsed.is_empty() {
        return Ok(());
    }
    // The whole server catalog, as queue-maintenance reads it: a retired job on
    // an organization's benchmark needs its entry for the failure record.
    let public = state.catalog_cache.get().await.map_err(|e| {
        AppError::internal(e.context("failed to load the catalog to settle a lapsed lease"))
    })?;
    let catalog = server_catalog(state.catalog_store.as_ref(), &public)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to load the catalog to settle a lapsed lease"))
        })?;
    for lease in lapsed {
        let outcome = settle_expired_lease(
            todo,
            &*state.submission_store,
            &catalog,
            lease,
            state.config.plan_lost_lease_limit,
            now,
        )
        .await
        .map_err(|e| AppError::internal(e.context("failed to settle the lapsed lease")))?;
        tracing::info!(
            client_id = %client_id,
            job_id = %lease.job_id,
            outcome = ?outcome,
            "lapsed own lease settled before a new lease"
        );
    }
    Ok(())
}

/// A device refused the job's vault weights cannot run it, so the grant goes
/// back to `avail/` for one that can. A failed check reverts without a denial:
/// the fault may be the store's.
async fn revert_claim_if_out_of_vault_reach(
    state: &AppState,
    client_id: &ClientId,
    job_id: &JobId,
    body: &serde_json::Value,
    expires_at: ExpiresAt,
    lease_expiry: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<bool, AppError> {
    let denied_at = match client_may_fetch_job(state, client_id, body).await {
        Ok(true) => return Ok(false),
        Ok(false) => Some(now),
        Err(e) => {
            tracing::error!(error = %e, "vault check failed; grant reverted");
            None
        }
    };
    put_back_out_of_vault_reach(
        state,
        client_id,
        job_id,
        expires_at,
        lease_expiry,
        denied_at,
    )
    .await?;
    Ok(true)
}

/// Puts the lease back in `avail/`. A denial, when given, keeps the client's
/// later polls off the job.
async fn put_back_out_of_vault_reach(
    state: &AppState,
    client_id: &ClientId,
    job_id: &JobId,
    expires_at: ExpiresAt,
    lease_expiry: DateTime<Utc>,
    denied_at: Option<DateTime<Utc>>,
) -> Result<(), AppError> {
    // Durable before the recycle, as a retriable failure's is.
    if let Some(now) = denied_at {
        state
            .todo_store
            .write_denied(
                job_id,
                client_id,
                &DenialRecord::new(
                    now,
                    VAULT_OUT_OF_REACH,
                    FailureCode::Unspecified,
                    None,
                    None,
                ),
            )
            .await
            .map_err(|e| AppError::internal(e.context("failed to deny the client the job")))?;
    }
    let resolution = pipette_pipeline::queue_maintenance::resolve_or_recycle_lease(
        &*state.todo_store,
        &*state.submission_store,
        job_id,
        expires_at,
        client_id,
        lease_expiry,
    )
    .await
    .map_err(|e| AppError::internal(e.context("failed to put the lease back")))?;
    tracing::warn!(
        client_id = %client_id,
        job_id = %job_id,
        resolution = ?resolution,
        reason = "vault_out_of_reach",
        "lease put back: the job names a vault resource the client may not fetch, or the check failed"
    );
    Ok(())
}

/// Checked before the rename. The lease of a device refused the job's vault
/// weights goes back to `avail/`, denied to it, for one that can fetch them. A
/// failed check answers `500` and keeps the lease: the run may be fine.
async fn refuse_renewal_out_of_vault_reach(
    state: &AppState,
    client_id: &ClientId,
    job_id: &JobId,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    let todo = state.todo_store.as_ref();
    // shortcut: an extra leased/ LIST and GET on every renewal, vault job or
    // not; mark vault jobs at claim if heartbeat load shows it.
    let leased = own_leased_body(todo, client_id, job_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the leased job body")))?;
    let Some((expires_at, lease_expiry, body)) = leased else {
        return Ok(());
    };
    if client_may_fetch_job(state, client_id, &body).await? {
        return Ok(());
    }
    // A renewal since the read leaves a stale key, which the put-back answers `Gone`.
    put_back_out_of_vault_reach(
        state,
        client_id,
        job_id,
        expires_at,
        lease_expiry,
        Some(now),
    )
    .await?;
    tracing::warn!(
        reason = "vault_out_of_reach",
        "refused renewal: the job names a vault resource the client may not fetch"
    );
    Err(AppError::Forbidden(VAULT_OUT_OF_REACH.into()))
}

fn iso8601_duration(secs: u64) -> String {
    if secs.is_multiple_of(60) {
        format!("PT{}M", secs / 60)
    } else {
        format!("PT{secs}S")
    }
}

/// `benchmark_id` is lifted from `spec.benchmark` so the client's
/// envelope-versus-spec check passes by construction.
///
/// Best-effort: the job is already leased, so a `500` would strand the lease
/// with the client never learning the `job_id`; answer with what there is.
fn claim_response(
    body: &serde_json::Value,
    expires_at: ExpiresAt,
    secs: u64,
    client_versions: Vec<String>,
) -> ClaimAnswer {
    let field = |name: &str| body.get(name).cloned();
    let spec = field("spec").filter(|s| !s.is_null());
    let job_id = field("job_id");
    // Dropped unless a string, so the warning below sees a number as missing.
    let benchmark = spec
        .as_ref()
        .and_then(|s| s.get("benchmark"))
        .filter(|b| b.is_string())
        .cloned();
    if !matches!(job_id, Some(serde_json::Value::String(_))) || benchmark.is_none() {
        tracing::warn!(
            job_id = ?job_id,
            "leasing out a job whose claim envelope is undecodable (needs a string \
             `job_id` and `spec.benchmark`) — the client cannot run it or report it \
             terminally, so the lease will lapse and the job will be re-served \
             indefinitely; ingestion refuses such a body, so this one was written \
             straight into avail/"
        );
    }

    // From the `avail/` key, not the body: the key is the deadline every
    // scheduling path enforces. `never` is omitted rather than sent as a literal.
    let deadline = matches!(expires_at, ExpiresAt::At(_)).then(|| expires_at.to_string());
    match field("expires_at") {
        Some(serde_json::Value::String(s)) if s.parse::<ExpiresAt>().ok() != Some(expires_at) => {
            tracing::warn!(
                job_id = ?job_id,
                body_expires_at = %s,
                key_expires_at = %expires_at,
                "job body's `expires_at` disagrees with its `avail/` key; the key wins and \
                 the claim response carries it — either queue-maintenance re-dated the job, \
                 or the body is wrong at its source"
            )
        }
        Some(v) if !v.is_string() && !v.is_null() => tracing::warn!(
            job_id = ?job_id,
            key_expires_at = %expires_at,
            "job body's `expires_at` is not a string; the key wins and the claim response \
             carries it — fix the body at its source"
        ),
        _ => {}
    }
    let time_window = iso8601_duration(secs);
    match (job_id, benchmark) {
        (
            Some(serde_json::Value::String(job_id)),
            Some(serde_json::Value::String(benchmark_id)),
        ) => ClaimAnswer::Claimed(ClaimedJob {
            job_id,
            benchmark_id,
            time_window,
            expires_at: deadline,
            client_versions,
            spec,
        }),
        (job_id, benchmark_id) => ClaimAnswer::Undecodable(UndecodableClaim {
            job_id,
            benchmark_id,
            time_window,
            expires_at: deadline,
            client_versions,
            spec,
        }),
    }
}

/// The stage times of one claim, for the `"claim timing"` event.
#[derive(Debug, Default)]
struct ClaimTiming {
    /// The reads and settlements before the first transaction.
    gate_ms: u64,
    transaction_ms: u64,
    vault_check_ms: u64,
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl ClaimTiming {
    /// `INFO` for a slow claim, at the threshold of a slow job read; else `DEBUG`.
    fn log(&self, elapsed: Duration) {
        let total_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        macro_rules! emit {
            ($level:expr) => {
                tracing::event!(
                    $level,
                    total_ms,
                    gate_ms = self.gate_ms,
                    transaction_ms = self.transaction_ms,
                    vault_check_ms = self.vault_check_ms,
                    "claim timing"
                );
            };
        }
        if elapsed >= Duration::from_secs(1) {
            emit!(tracing::Level::INFO);
        } else {
            emit!(tracing::Level::DEBUG);
        }
    }
}

// POST /plans/claim
pub async fn claim(
    State(state): State<AppState>,
    WorkingClient(client): WorkingClient,
    audit: Audit,
) -> Result<Response, AppError> {
    let started = Instant::now();
    let mut timing = ClaimTiming::default();
    let response = claim_timed(&state, &client, &audit, &mut timing).await;
    timing.log(started.elapsed());
    response
}

async fn claim_timed(
    state: &AppState,
    client: &Client,
    audit: &Audit,
    timing: &mut ClaimTiming,
) -> Result<Response, AppError> {
    // Both refusals — status and permission — are the extractor's.
    let client_id = &client.client_id;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();
    let gate = Instant::now();

    // The flag means the eligible markers are stale; no work until
    // `queue-maintenance` clears it. Re-checked after the claim, too.
    if todo.has_pending_reindex(client_id).await.map_err(|e| {
        AppError::internal(e.context("failed to read the pending-reindex flag during claim"))
    })? {
        audit.unchanged();
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // Read before the transaction, so a failed read takes no lease.
    let tags = client_tags(state.tag_store.as_ref(), client_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to read the client tags during claim"))
        })?;
    timing.gate_ms = elapsed_ms(gate);

    let lease_secs = state.config.plan_lease_duration_secs.get();
    let lease_expiry = state.config.lease_expiry_from(now);
    let limit = state.config.plan_lost_lease_limit;

    // A job that a pass puts back stays out of the later passes.
    let mut passed_over = Vec::new();
    let mut checked_lease = None;
    loop {
        let transaction = Instant::now();
        let outcome = todo
            .claim_for_client(
                client_id,
                lease_expiry,
                limit,
                &passed_over,
                checked_lease.as_ref(),
                now,
            )
            .await
            .map_err(|e| AppError::internal(e.context("failed to claim a job")))?;
        timing.transaction_ms += elapsed_ms(transaction);
        let (lease, body, handed_back, check_held) = match outcome {
            ClaimOutcome::SettlementRequired => {
                settle_lapsed_leases(state, client_id, None, now).await?;
                continue;
            }
            ClaimOutcome::Empty => {
                audit.unchanged();
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
            ClaimOutcome::Withheld(lease) => {
                tracing::Span::current().record("job_id", tracing::field::display(&lease.job_id));
                audit
                    .involve(
                        state,
                        [AuditTarget::Job {
                            job_id: lease.job_id.clone(),
                        }],
                    )
                    .await;
                // Its lease lapses and `queue-maintenance` retires the job.
                tracing::warn!(
                    client_id = %client_id,
                    job_id = %lease.job_id,
                    limit,
                    "withheld a held job whose lease was lost too many times"
                );
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
            ClaimOutcome::CheckHeldLease { lease, body } => (lease, body, true, true),
            ClaimOutcome::HandedBack { lease, body } => (lease, body, true, false),
            ClaimOutcome::Granted { lease, body } => (lease, body, false, false),
        };
        tracing::Span::current().record("job_id", tracing::field::display(&lease.job_id));

        if handed_back {
            // The original grant's handler performs the post-grant flag check.
            if todo.has_pending_reindex(client_id).await.map_err(|e| {
                AppError::internal(e.context(
                    "failed to re-check the pending-reindex flag before handing back the job",
                ))
            })? {
                audit.unchanged();
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
        } else if revert_claim_if_void(todo, &*state.submission_store, &lease).await? {
            audit.unchanged();
            return Ok(StatusCode::NO_CONTENT.into_response());
        }

        if !handed_back || check_held {
            let vault_check = Instant::now();
            let put_back = revert_claim_if_out_of_vault_reach(
                state,
                client_id,
                &lease.job_id,
                &body,
                lease.expires_at,
                lease.lease_expiry,
                now,
            )
            .await?;
            timing.vault_check_ms += elapsed_ms(vault_check);
            if put_back {
                if handed_back {
                    audit.unchanged();
                    return Ok(StatusCode::NO_CONTENT.into_response());
                }
                passed_over.push(lease.job_id);
                continue;
            }
        }
        if check_held {
            // The store rechecks the exact lease before it counts the hand-back.
            checked_lease = Some(lease);
            continue;
        }

        audit
            .involve(
                state,
                [AuditTarget::Job {
                    job_id: lease.job_id.clone(),
                }],
            )
            .await;
        let versions = admitted_client_versions(&body, client, &tags);
        let window_secs = if handed_back {
            // The holder lost the claim response or restarted under the job.
            tracing::info!(
                client_id = %client_id,
                job_id = %lease.job_id,
                "idempotent claim: returned already-held lease to re-polling client"
            );
            u64::try_from((lease.lease_expiry - now).num_seconds()).unwrap_or(0)
        } else {
            tracing::info!(%lease_expiry, "job claimed");
            lease_secs
        };
        return Ok(Json(claim_response(
            &body,
            lease.expires_at,
            window_secs,
            versions,
        ))
        .into_response());
    }
}

// PUT /plans/{job_id}/heartbeat
pub async fn heartbeat(
    State(state): State<AppState>,
    WorkingClient(client): WorkingClient,
    ApiPath(Parsed(job_id)): ApiPath<Parsed<JobId>>,
) -> Result<Response, AppError> {
    let client_id = &client.client_id;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();

    let new_expiry = state.config.lease_expiry_from(now);

    // Refused without a rename, so a renewal cannot resurrect a lease out from
    // under the relinquish.
    if todo.has_pending_reindex(client_id).await.map_err(|e| {
        AppError::internal(e.context("failed to read the pending-reindex flag during heartbeat"))
    })? {
        tracing::warn!(
            client_id = %client_id,
            job_id = %job_id,
            reason = "pending_reindex",
            "refused heartbeat during pending reindex — the flag voids the client's queue standing until the next queue-maintenance run"
        );
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    refuse_renewal_out_of_vault_reach(&state, client_id, &job_id, now).await?;

    match todo
        .renew_lease(&job_id, client_id, new_expiry)
        .await
        .map_err(|e| AppError::internal(e.context("failed to renew the lease during heartbeat")))?
    {
        RenewLeaseResult::Renewed(_) => Ok(StatusCode::OK.into_response()),
        RenewLeaseResult::NotFound => Ok(StatusCode::NOT_FOUND.into_response()),
        RenewLeaseResult::WrongClient => {
            tracing::warn!(
                reason = "leased_elsewhere",
                "refused heartbeat: the job is leased to another client"
            );
            Ok(StatusCode::CONFLICT.into_response())
        }
    }
}

// POST /plans/{job_id}/reclaim
pub async fn reclaim(
    State(state): State<AppState>,
    WorkingClient(client): WorkingClient,
    ApiPath(Parsed(job_id)): ApiPath<Parsed<JobId>>,
) -> Result<Response, AppError> {
    let client_id = &client.client_id;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();

    let new_expiry = state.config.lease_expiry_from(now);

    // Gates the renewal too, so the rename cannot resurrect a relinquished
    // lease. Re-checked after the re-acquire.
    if todo.has_pending_reindex(client_id).await.map_err(|e| {
        AppError::internal(e.context("failed to read the pending-reindex flag during reclaim"))
    })? {
        tracing::warn!(
            client_id = %client_id,
            job_id = %job_id,
            reason = "pending_reindex",
            "refused reclaim during pending reindex — the flag voids the client's queue standing; prior work is forfeited"
        );
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    refuse_renewal_out_of_vault_reach(&state, client_id, &job_id, now).await?;
    loop {
        let outcome = todo
            .reclaim_for_client(
                &job_id,
                client_id,
                new_expiry,
                state.config.plan_lost_lease_limit,
                now,
            )
            .await
            .map_err(|e| AppError::internal(e.context("failed to reclaim the job")))?;
        let response = match outcome {
            ReclaimOutcome::SettlementRequired => {
                settle_lapsed_leases(&state, client_id, Some(&job_id), now).await?;
                continue;
            }
            ReclaimOutcome::Renewed => {
                tracing::info!(lease_expiry = %new_expiry, "lease renewed on reclaim");
                Ok(StatusCode::OK.into_response())
            }
            ReclaimOutcome::LeasedElsewhere => {
                tracing::warn!(
                    reason = "leased_elsewhere",
                    "refused reclaim: the job is leased to another client"
                );
                Ok(StatusCode::CONFLICT.into_response())
            }
            ReclaimOutcome::Refused => Ok(StatusCode::NOT_FOUND.into_response()),
            ReclaimOutcome::Granted { lease, body } => {
                if revert_claim_if_void(todo, &*state.submission_store, &lease).await? {
                    return Ok(StatusCode::NOT_FOUND.into_response());
                }
                if revert_claim_if_out_of_vault_reach(
                    &state,
                    client_id,
                    &lease.job_id,
                    &body,
                    lease.expires_at,
                    lease.lease_expiry,
                    now,
                )
                .await?
                {
                    return Err(AppError::Forbidden(VAULT_OUT_OF_REACH.into()));
                }
                tracing::info!(lease_expiry = %new_expiry, "job reclaimed from avail/");
                Ok(StatusCode::OK.into_response())
            }
        };
        return response;
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_core::config::StorageConfig;
    use pipette_store_fs::{LocalFsSubmissionStore, build_local_fs_todo_store};
    use pipette_store_testkit::seed_leased_until;

    use super::*;

    #[rstest]
    #[case::flag_up_reverts(true)]
    #[case::no_flag_keeps_lease(false)]
    #[tokio::test]
    async fn revert_claim_only_when_void(#[case] flagged: bool) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        };
        let todo = build_local_fs_todo_store(&config)?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));

        let job = JobId::new_unchecked("job-revert");
        let client = ClientId::try_new("revert-client")?;
        let lease = Lease {
            job_id: job.clone(),
            expires_at: ExpiresAt::Never,
            client_id: client.clone(),
            lease_expiry: Utc::now() + chrono::Duration::hours(1),
        };
        seed_leased_until(
            &*todo,
            job.as_str(),
            lease.expires_at,
            &client,
            lease.lease_expiry,
            &serde_json::json!({ "job_id": job.as_str() }),
        )
        .await?;
        if flagged {
            todo.write_pending_reindex(&client).await?;
        }

        let reverted = revert_claim_if_void(&*todo, &submissions, &lease).await?;

        assert_eq!(reverted, flagged);
        assert_eq!(todo.get_avail_by_job(&job).await?.is_some(), flagged);
        let still_held = todo
            .leases_for_client(&client)
            .await?
            .iter()
            .any(|held| held.job_id == job);
        assert_eq!(still_held, !flagged);
        Ok(())
    }

    #[test]
    fn a_claim_answers_with_the_shared_envelope() -> anyhow::Result<()> {
        let body = serde_json::json!({
            "job_id": "job-1",
            "spec": {"benchmark": "gsm8k", "model": {}},
        });
        let ClaimAnswer::Claimed(claimed) =
            claim_response(&body, ExpiresAt::Never, 300, vec!["1.2.0".into()])
        else {
            anyhow::bail!("a well-formed body answers decodably");
        };
        assert_eq!(claimed.job_id, "job-1");
        assert_eq!(claimed.benchmark_id, "gsm8k");
        assert_eq!(claimed.time_window, "PT5M");
        assert_eq!(claimed.expires_at, None);
        assert_eq!(claimed.client_versions, ["1.2.0"]);
        Ok(())
    }

    /// A body written straight into `avail/` without a string `job_id` or
    /// `spec.benchmark` keeps the keys it can fill and leaves the rest out.
    #[rstest]
    #[case::no_benchmark(serde_json::json!({"job_id": "job-1", "spec": {}}), Some(serde_json::json!("job-1")), None)]
    #[case::numeric_job_id(serde_json::json!({"job_id": 7, "spec": {"benchmark": "gsm8k"}}), Some(serde_json::json!(7)), Some(serde_json::json!("gsm8k")))]
    #[case::no_job_id(serde_json::json!({"spec": {"benchmark": "gsm8k"}}), None, Some(serde_json::json!("gsm8k")))]
    fn an_undecodable_body_keeps_what_it_has(
        #[case] body: serde_json::Value,
        #[case] job_id: Option<serde_json::Value>,
        #[case] benchmark_id: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let answered =
            serde_json::to_value(claim_response(&body, ExpiresAt::Never, 300, Vec::new()))?;
        assert_eq!(answered.get("job_id"), job_id.as_ref());
        assert_eq!(answered.get("benchmark_id"), benchmark_id.as_ref());
        assert_eq!(
            answered.get("time_window"),
            Some(&serde_json::json!("PT5M"))
        );
        assert!(serde_json::from_value::<ClaimedJob>(answered).is_err());
        Ok(())
    }
}
