//! The work queue a device drives: `POST /plans/claim`,
//! `PUT /plans/{job_id}/heartbeat` and `POST /plans/{job_id}/reclaim`.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use getrandom::SysRng;
use getrandom::rand_core::TryRng;

use pipette_core::denial::DenialRecord;
use pipette_core::matching::admitted_client_versions;
use pipette_core::todo_filename::{Lease, parse_avail_filename, parse_leased_key};
use pipette_pipeline::queue_maintenance::client_tags;
use pipette_plan_types::{
    AuditTarget, ClaimAnswer, ClaimedJob, ClientId, ExpiresAt, JobId, UndecodableClaim,
};
use pipette_store::{ClaimResult, RenewLeaseResult, SubmissionStore, TodoStore};

use crate::audit::Audit;
use crate::auth::WorkingClient;
use crate::client::benchmarks::submissions::own_leased_body;
use crate::error::AppError;
use crate::extract::{ApiPath, Parsed};
use crate::state::AppState;
use crate::vault_access::client_may_fetch_job;

const VAULT_OUT_OF_REACH: &str = "the job names a vault resource this client may not fetch";

/// Every grant and renewal renames before it lists, so the later of two racing
/// ones sees both leases. Returns the other job the client was suspended over.
async fn suspend_if_holding_another_lease(
    todo: &dyn TodoStore,
    client_id: &ClientId,
    job_id: &JobId,
    now: DateTime<Utc>,
) -> Result<Option<JobId>, AppError> {
    let Some(other) = todo
        .list_leased_for_client(client_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the client's leases")))?
        .iter()
        .filter_map(|name| parse_leased_key(name).ok())
        .find(|lease| &lease.job_id != job_id && lease.lease_expiry > now)
    else {
        return Ok(None);
    };
    tracing::warn!(
        client_id = %client_id,
        job_id = %job_id,
        conflicting_job_id = %other.job_id,
        "suspending client: it holds more than one live lease"
    );
    todo.write_suspension(client_id, now, &other.job_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context(format!(
                "failed to suspend the client over its lease on job {}",
                other.job_id
            )))
        })?;
    Ok(Some(other.job_id))
}

/// Why [`revert_claim_if_void`] put a grant back.
#[derive(Debug, PartialEq)]
enum Reverted {
    PendingReindex,
    /// The client is suspended over its live lease on this other job.
    SecondLease(JobId),
}

/// The gate and the rename are not atomic, so a PATCH's relinquish can miss a
/// lease created after its listing. Flag visible here: this side reverts it;
/// not visible: the relinquish lists it. Exactly one party cleans up.
/// A grant that leaves the client a second live lease is reverted too.
async fn revert_claim_if_void(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    client_id: &ClientId,
    job_id: &JobId,
    expires_at: ExpiresAt,
    lease_expiry: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Option<Reverted>, AppError> {
    let reason = if todo.has_pending_reindex(client_id).await.map_err(|e| {
        AppError::internal(
            e.context("failed to re-check the pending-reindex flag after granting the job"),
        )
    })? {
        Reverted::PendingReindex
    } else if let Some(other) =
        suspend_if_holding_another_lease(todo, client_id, job_id, now).await?
    {
        Reverted::SecondLease(other)
    } else {
        return Ok(None);
    };
    // Every resolution counts as reverted, Gone included: a racing relinquish
    // resolved it, or (second lease) the client's own renewal moved it and the
    // suspension lets it lapse.
    let resolution = pipette_pipeline::queue_maintenance::resolve_or_recycle_lease(
        todo,
        submissions,
        job_id,
        expires_at,
        client_id,
        lease_expiry,
    )
    .await
    .map_err(|e| AppError::internal(e.context("failed to revert the claim")))?;
    tracing::warn!(
        client_id = %client_id,
        job_id = %job_id,
        resolution = ?resolution,
        reason = ?reason,
        "lease reverted: the pending-reindex flag rose after the gate, or the client holds another lease"
    );
    Ok(Some(reason))
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
                &DenialRecord::new(now, VAULT_OUT_OF_REACH, None, None),
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

// POST /plans/claim
pub async fn claim(
    State(state): State<AppState>,
    WorkingClient(client): WorkingClient,
    audit: Audit,
) -> Result<Response, AppError> {
    // Both refusals — status and permission — are the extractor's.
    let client_id = &client.client_id;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();

    if todo
        .read_suspension(client_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to read the suspension record during claim"))
        })?
        .is_some()
    {
        audit.unchanged();
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // The flag means the eligible markers are stale; no work until
    // `queue-maintenance` clears it. Re-checked after the claim, too.
    if todo.has_pending_reindex(client_id).await.map_err(|e| {
        AppError::internal(e.context("failed to read the pending-reindex flag during claim"))
    })? {
        audit.unchanged();
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // `own_live` is keyed by job: a listing can surface both keys of a renewal
    // rename, and counting them twice would falsely suspend the client.
    let leased = todo
        .list_leased()
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the leases to select a job")))?;
    let mut taken: HashSet<JobId> = HashSet::new();
    let mut own_live: HashMap<JobId, (ExpiresAt, DateTime<Utc>)> = HashMap::new();
    for name in &leased {
        let Ok(Lease {
            job_id,
            expires_at,
            client_id: lease_client,
            lease_expiry,
        }) = parse_leased_key(name)
        else {
            continue;
        };
        if lease_expiry <= now {
            continue;
        }
        if &lease_client == client_id {
            own_live
                .entry(job_id)
                .and_modify(|e| {
                    if lease_expiry > e.1 {
                        *e = (expires_at, lease_expiry);
                    }
                })
                .or_insert((expires_at, lease_expiry));
        } else {
            taken.insert(job_id);
        }
    }

    // A client holds at most one lease; more is an anomaly and suspends it.
    if own_live.len() > 1 {
        if let Some(conflicting) = own_live.keys().next() {
            tracing::warn!(
                client_id = %client_id,
                conflicting_job_id = %conflicting,
                lease_count = own_live.len(),
                "suspending client: claimed while holding multiple live leases"
            );
            todo.write_suspension(client_id, now, conflicting)
                .await
                .map_err(|e| {
                    AppError::internal(e.context(format!(
                        "failed to suspend the client over its lease on job {conflicting}"
                    )))
                })?;
            audit
                .involve(
                    &state,
                    [AuditTarget::Job {
                        job_id: conflicting.clone(),
                    }],
                )
                .await;
        }
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // Read only once there is a job, and before any rename, so a failed read
    // takes no lease.
    let read_tags = || async {
        client_tags(state.tag_store.as_ref(), client_id)
            .await
            .map_err(|e| {
                AppError::internal(e.context("failed to read the client tags during claim"))
            })
    };

    if let Some((job_id, (expires_at, lease_expiry))) = own_live.iter().next() {
        tracing::Span::current().record("job_id", tracing::field::display(job_id));
        // Handed back without renewing the lease, so a crash-looping client
        // still lapses on schedule.
        if let Some(body) = todo
            .get_leased(job_id, *expires_at, client_id, *lease_expiry)
            .await
            .map_err(|e| {
                AppError::internal(
                    e.context("failed to read the leased body of the job the client already holds"),
                )
            })?
        {
            // No revert needed: the lease is already in `leased/`, so the
            // racing relinquish lists and resolves it.
            if todo.has_pending_reindex(client_id).await.map_err(|e| {
                AppError::internal(e.context(
                    "failed to re-check the pending-reindex flag before handing back the job",
                ))
            })? {
                audit.unchanged();
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
            if revert_claim_if_out_of_vault_reach(
                &state,
                client_id,
                job_id,
                &body,
                *expires_at,
                *lease_expiry,
                now,
            )
            .await?
            {
                audit.unchanged();
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
            audit
                .involve(
                    &state,
                    [AuditTarget::Job {
                        job_id: job_id.clone(),
                    }],
                )
                .await;
            // The holder lost the claim response or restarted under the job, so
            // count this lease as lost. A job out of losses is withheld; its
            // lease lapses and `queue-maintenance` retires it.
            let limit = state.config.plan_lost_lease_limit;
            if limit > 0 {
                todo.write_lost_lease(job_id, client_id, *lease_expiry)
                    .await
                    .map_err(|e| {
                        AppError::internal(
                            e.context("failed to record the re-polled lease as lost"),
                        )
                    })?;
                let losses = todo.list_lost_leases(job_id).await.map_err(|e| {
                    AppError::internal(e.context("failed to list the job's lost leases"))
                })?;
                if losses.len() >= limit {
                    tracing::warn!(
                        client_id = %client_id,
                        job_id = %job_id,
                        losses = losses.len(),
                        "withheld a held job whose lease was lost too many times"
                    );
                    return Ok(StatusCode::NO_CONTENT.into_response());
                }
            }
            let remaining_secs = u64::try_from((*lease_expiry - now).num_seconds()).unwrap_or(0);
            let tags = read_tags().await?;
            let versions = admitted_client_versions(&body, &client, &tags);
            let body = claim_response(&body, *expires_at, remaining_secs, versions);
            tracing::info!(
                client_id = %client_id,
                job_id = %job_id,
                "idempotent claim: returned already-held lease to re-polling client"
            );
            return Ok(Json(body).into_response());
        }
    }

    let candidates = todo
        .list_eligible_for_client(client_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to list the eligible jobs during claim"))
        })?;
    if candidates.is_empty() {
        audit.unchanged();
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let tags = read_tags().await?;

    // Soonest expiry first; the random tiebreak keeps clients from stampeding
    // one job in a tier a planner run stamped with a single expiry.
    let mut ranked: Vec<(JobId, ExpiresAt, u64)> = candidates
        .into_iter()
        // Without the expiry filter the sort would prefer an unswept expired job.
        .filter(|(job_id, exp)| !taken.contains(job_id) && !exp.is_expired(now))
        .map(|(job_id, exp)| {
            SysRng
                .try_next_u64()
                .map(|tiebreak| (job_id, exp, tiebreak))
        })
        .collect::<Result<_, _>>()?;
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));

    let lease_secs = state.config.plan_lease_duration_secs.get();
    let lease_expiry = state.config.lease_expiry_from(now);

    for (job_id, expires_at, _) in &ranked {
        tracing::Span::current().record("job_id", tracing::field::display(job_id));
        // A point read per candidate, not a listing of all of `denied/`.
        if todo.is_denied(job_id, client_id).await.map_err(|e| {
            AppError::internal(e.context("failed to read the denial marker during claim"))
        })? {
            continue;
        }
        // A miss falls through to the next candidate: another claimer took the
        // job between the LIST and the rename, or it left the queue.
        let Some((granted, body)) =
            claim_at_current_key(todo, client_id, job_id, *expires_at, lease_expiry, now)
                .await
                .map_err(|e| AppError::internal(e.context("failed to claim the job")))?
        else {
            continue;
        };
        match revert_claim_if_void(
            todo,
            &*state.submission_store,
            client_id,
            job_id,
            granted,
            lease_expiry,
            now,
        )
        .await?
        {
            None => {}
            Some(Reverted::PendingReindex) => {
                audit.unchanged();
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
            Some(Reverted::SecondLease(conflicting)) => {
                audit
                    .involve(
                        &state,
                        [AuditTarget::Job {
                            job_id: conflicting,
                        }],
                    )
                    .await;
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
        }
        if revert_claim_if_out_of_vault_reach(
            &state,
            client_id,
            job_id,
            &body,
            granted,
            lease_expiry,
            now,
        )
        .await?
        {
            continue;
        }
        audit
            .involve(
                &state,
                [AuditTarget::Job {
                    job_id: job_id.clone(),
                }],
            )
            .await;
        tracing::info!(%lease_expiry, "job claimed");
        let versions = admitted_client_versions(&body, &client, &tags);
        return Ok(Json(claim_response(&body, granted, lease_secs, versions)).into_response());
    }

    audit.unchanged();
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A re-date ([`redate_avail`]) landing mid-request leaves the marker naming a
/// gone key, so a miss retries once under the job's current key.
///
/// [`redate_avail`]: pipette_store::TodoStore::redate_avail
async fn claim_at_current_key(
    todo: &dyn TodoStore,
    client_id: &ClientId,
    job_id: &JobId,
    marker_expires_at: ExpiresAt,
    lease_expiry: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<(ExpiresAt, serde_json::Value)>> {
    if let ClaimResult::Claimed(body) = todo
        .claim_job(job_id, marker_expires_at, client_id, lease_expiry)
        .await?
    {
        return Ok(Some((marker_expires_at, body)));
    }
    let Some(current) = todo
        .avail_key_for_job(job_id)
        .await?
        .and_then(|name| parse_avail_filename(&name).ok())
        .map(|(_, expires_at)| expires_at)
    else {
        return Ok(None);
    };
    // The same key, or one already past: nothing a retry can win.
    if current == marker_expires_at || current.is_expired(now) {
        return Ok(None);
    }
    let ClaimResult::Claimed(body) = todo
        .claim_job(job_id, current, client_id, lease_expiry)
        .await?
    else {
        return Ok(None);
    };
    tracing::warn!(
        client_id = %client_id,
        job_id = %job_id,
        marker_expires_at = %marker_expires_at,
        key_expires_at = %current,
        "claimed a job under the deadline its avail/ key carries; the client's eligible marker \
         names an older one, which the next queue-maintenance run collects"
    );
    Ok(Some((current, body)))
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

    // Refused so the suspended client's leases lapse on their own.
    if todo
        .read_suspension(client_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to read the suspension record during heartbeat"))
        })?
        .is_some()
    {
        tracing::warn!(
            reason = "suspended",
            "refused heartbeat: the client is suspended"
        );
        return Err(AppError::Forbidden("client is suspended".into()));
    }

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
        RenewLeaseResult::Renewed(_) => {
            // Renewing a lapsed lease can leave the client a second live one.
            if suspend_if_holding_another_lease(todo, client_id, &job_id, now)
                .await?
                .is_some()
            {
                return Err(AppError::Forbidden("client is suspended".into()));
            }
            Ok(StatusCode::OK.into_response())
        }
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

    // `403`, not claim's `204`: a reclaim names a job, so the refusal is reported.
    if todo
        .read_suspension(client_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to read the suspension record during reclaim"))
        })?
        .is_some()
    {
        tracing::warn!(
            reason = "suspended",
            "refused reclaim: the client is suspended"
        );
        return Err(AppError::Forbidden("client is suspended".into()));
    }

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

    // Ahead of the eligibility and expiry gates: a held job may finish past its
    // `expires_at`, and a stale eligible index must not yank it.
    match todo
        .renew_lease(&job_id, client_id, new_expiry)
        .await
        .map_err(|e| AppError::internal(e.context("failed to renew the lease during reclaim")))?
    {
        RenewLeaseResult::Renewed(_) => {
            if suspend_if_holding_another_lease(todo, client_id, &job_id, now)
                .await?
                .is_some()
            {
                return Err(AppError::Forbidden("client is suspended".into()));
            }
            tracing::info!(lease_expiry = %new_expiry, "lease renewed on reclaim");
            return Ok(StatusCode::OK.into_response());
        }
        RenewLeaseResult::WrongClient => {
            tracing::warn!(
                reason = "leased_elsewhere",
                "refused reclaim: the job is leased to another client"
            );
            return Ok(StatusCode::CONFLICT.into_response());
        }
        RenewLeaseResult::NotFound => {}
    }

    let Some((_, expires_at)) = todo
        .list_eligible_for_client(client_id)
        .await
        .map_err(|e| {
            AppError::internal(e.context("failed to list the eligible jobs during reclaim"))
        })?
        .into_iter()
        .find(|(candidate, _)| candidate == &job_id)
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    if expires_at.is_expired(now) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    if todo.is_denied(&job_id, client_id).await.map_err(|e| {
        AppError::internal(e.context("failed to read the denial marker during reclaim"))
    })? {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    match todo
        .claim_job(&job_id, expires_at, client_id, new_expiry)
        .await
        .map_err(|e| AppError::internal(e.context("failed to re-acquire the job")))?
    {
        ClaimResult::Claimed(body) => {
            if revert_claim_if_void(
                todo,
                &*state.submission_store,
                client_id,
                &job_id,
                expires_at,
                new_expiry,
                now,
            )
            .await?
            .is_some()
            {
                return Ok(StatusCode::NOT_FOUND.into_response());
            }
            if revert_claim_if_out_of_vault_reach(
                &state, client_id, &job_id, &body, expires_at, new_expiry, now,
            )
            .await?
            {
                return Err(AppError::Forbidden(VAULT_OUT_OF_REACH.into()));
            }
            tracing::info!(lease_expiry = %new_expiry, "job reclaimed from avail/");
            Ok(StatusCode::OK.into_response())
        }
        ClaimResult::Gone => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_core::config::StorageConfig;
    use pipette_core::todo_filename::leased_key;
    use pipette_store_fs::{LocalFsSubmissionStore, build_local_fs_todo_store};

    use super::*;

    /// The other lease stands in for a concurrent claim that won a second job.
    #[rstest]
    #[case::flag_up_reverts(true, None, Some(Reverted::PendingReindex))]
    #[case::no_flag_keeps_lease(false, None, None)]
    #[case::another_live_lease_reverts_and_suspends(
        false,
        Some(1),
        Some(Reverted::SecondLease(JobId::new_unchecked("job-other")))
    )]
    #[case::another_lapsed_lease_keeps_lease(false, Some(-1), None)]
    #[tokio::test]
    async fn revert_claim_only_when_void(
        #[case] flagged: bool,
        #[case] other_lease_hours: Option<i64>,
        #[case] expect: Option<Reverted>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        };
        let todo = build_local_fs_todo_store(&config)?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));

        let job = JobId::new_unchecked("job-revert");
        let other = JobId::new_unchecked("job-other");
        let client = ClientId::try_new("revert-client")?;
        let now = Utc::now();
        let lease_expiry = now + chrono::Duration::hours(1);
        let seed = |job: &JobId, expiry: DateTime<Utc>| -> anyhow::Result<()> {
            let path = dir.path().join("todo").join("leased").join(leased_key(
                job,
                ExpiresAt::Never,
                &client,
                expiry,
            ));
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("leased key has no parent dir"))?;
            std::fs::create_dir_all(parent)?;
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({ "job_id": job.as_str() }))?,
            )?;
            Ok(())
        };
        seed(&job, lease_expiry)?;
        other_lease_hours
            .map(|hours| seed(&other, now + chrono::Duration::hours(hours)))
            .transpose()?;
        if flagged {
            todo.write_pending_reindex(&client).await?;
        }

        let reverted = revert_claim_if_void(
            &*todo,
            &submissions,
            &client,
            &job,
            ExpiresAt::Never,
            lease_expiry,
            now,
        )
        .await?;

        let expect_reverted = expect.is_some();
        assert_eq!(reverted, expect);
        assert_eq!(
            todo.get_avail_by_job(&job).await?.is_some(),
            expect_reverted
        );
        let still_held = todo
            .list_leased_for_client(&client)
            .await?
            .iter()
            .filter_map(|name| parse_leased_key(name).ok())
            .any(|lease| lease.job_id == job);
        assert_eq!(still_held, !expect_reverted);
        assert_eq!(
            todo.read_suspension(&client).await?.is_some(),
            matches!(expect, Some(Reverted::SecondLease(_)))
        );
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
