//! HTTP handlers for the planner job-queue endpoints:
//! `POST /plans/claim`, `PUT /plans/{job_id}/heartbeat`, and
//! `POST /plans/{job_id}/reclaim`.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};

use crate::auth::AuthenticatedClient;
use crate::client::ClientStatus;
use crate::error::AppError;
use crate::handlers::AppState;
use crate::stores::{ClaimResult, RenewLeaseResult, SubmissionStore, TodoStore};
use crate::todo_filename::parse_leased_key;
use crate::types::{ClientId, ExpiresAt, JobId};

fn internal(e: anyhow::Error) -> AppError {
    AppError::Internal(e.to_string())
}

/// Re-check the pending-reindex flag after a successful `claim_job` and undo
/// the claim when it is up, returning `true` when the lease was reverted.
///
/// The flag gate at the top of `claim`/`reclaim` and the `avail/ → leased/`
/// rename are not atomic: a device-profile PATCH landing between them runs
/// its relinquish while this request's lease does not exist yet, so the
/// relinquish's re-list can never see it (unlike a renewal, which renames a
/// key already visible in `leased/`). Ungated, the client would be left
/// holding a live lease granted from markers its new profile supersedes —
/// renewable once the flag clears, and reconciled by no maintenance pass
/// (`relinquish_client_leases` documents the renewal side of this argument).
/// The recheck makes the two orderings converge: the flag visible here means
/// this lease may predate the relinquish's listing, so it is resolved right
/// back; the flag *not* visible means its write — and therefore the
/// relinquish's `leased/` listing — strictly follows the rename, and the
/// relinquish resolves the lease itself. Either way exactly one party cleans
/// up.
async fn revert_claim_if_pending_reindex(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    client_id: &ClientId,
    job_id: &JobId,
    lease_expiry: DateTime<Utc>,
) -> Result<bool, AppError> {
    if !todo
        .has_pending_reindex(client_id)
        .await
        .map_err(internal)?
    {
        return Ok(false);
    }
    // Every resolution counts as reverted: `Recycled` returned the job to
    // `avail/`, `ResolvedStale` deleted a lease whose job already has a
    // record, and `Gone` means another actor (the racing relinquish itself)
    // already resolved it.
    let resolution = crate::queue_maintenance::resolve_or_recycle_lease(
        todo,
        submissions,
        job_id,
        client_id,
        lease_expiry,
    )
    .await
    .map_err(internal)?;
    tracing::warn!(
        client_id = %client_id,
        job_id = %job_id,
        resolution = ?resolution,
        "claim raced a device-profile change: pending-reindex flag rose after the gate; lease reverted"
    );
    Ok(true)
}

/// Format a whole-second lease increment as an ISO 8601 duration: `PT5M`
/// when the value is a whole number of minutes, otherwise `PT300S`. Both
/// forms are valid ISO 8601 and parseable by clients.
fn iso8601_duration(secs: u64) -> String {
    if secs.is_multiple_of(60) {
        format!("PT{}M", secs / 60)
    } else {
        format!("PT{secs}S")
    }
}

/// Project a stored job envelope into the claim response (`docs/httpapi.md`
/// §2.9.2): the server-owned fields a client acts on, wrapped around the
/// plan-authored `spec` passed through verbatim.
///
/// The envelope's scheduling fields — `requires`, `any_of`, `clients` — are
/// deliberately **not** forwarded. They are inputs to selection, already spent
/// by the time a job is handed out, and a device has no use for the roster it
/// was picked from.
///
/// `benchmark_id` is lifted from `spec.benchmark` rather than carried as a
/// second stored copy, so the client's envelope-versus-spec agreement check
/// (`pipette-clients`' `UnrunnableClaim::BenchmarkMismatch`) passes by
/// construction instead of depending on two fields staying in step.
///
/// `secs` becomes `time_window`, telling the client to heartbeat at half that
/// interval.
///
/// Every field is best-effort. Ingestion guarantees `job_id` and a `spec`
/// carrying `benchmark` (`plan_ingestion::validate_job`), but `todo/` accepts
/// writes from any planner (`docs/planner.md`), and by the time this runs the job
/// is already leased — so a `500` would strand the lease with the client never
/// learning the `job_id` it must reclaim. Handing back what there is at least
/// keeps the job addressable.
///
/// Two degrees of degradation, and only one of them is recoverable. A body whose
/// `spec` is present but unreadable still decodes as an envelope, so the client
/// reports it unrunnable and terminally, and the job is torn down. A body missing
/// `job_id` or `spec.benchmark` yields an envelope the client cannot decode at
/// all — it never learns the `job_id`, so it can neither reclaim nor report a
/// terminal failure, and the lease lapses, recycles, and is re-served
/// indefinitely. Only a direct `avail/` write reaches that state; the warning
/// below is the one thread leading back to the offending body, since the symptom
/// is a fleet making no progress rather than an error. The durable fix is
/// refusing such a body at its source, which ingestion already does.
fn claim_response(body: &serde_json::Value, secs: u64) -> serde_json::Value {
    let field = |name: &str| body.get(name).cloned();
    // Absent and explicitly-null `spec` collapse to the same thing — omitted from
    // the response, like every other field this projection cannot fill. (Clients
    // default the field to null when it is absent, so the two are indistinguishable
    // downstream; this is for internal consistency, not for the wire.)
    let spec = field("spec").filter(|s| !s.is_null());
    let job_id = field("job_id");
    // Dropped unless it is a string: `benchmark_id` is typed `String` on the
    // client, so forwarding a number here would fail the decode just as surely as
    // omitting it, without the omission being visible in the warning below.
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

    let mut envelope = serde_json::Map::new();
    if let Some(job_id) = job_id {
        envelope.insert("job_id".to_string(), job_id);
    }
    if let Some(benchmark) = benchmark {
        envelope.insert("benchmark_id".to_string(), benchmark);
    }
    envelope.insert(
        "time_window".to_string(),
        serde_json::Value::String(iso8601_duration(secs)),
    );
    // Round-tripped through `ExpiresAt` rather than forwarded as found, because the
    // wire contract is ISO 8601 *basic* format (`docs/httpapi.md` §2.9.2) while
    // ingestion *accepts* RFC 3339 at the handoff — so RFC 3339 is exactly what a
    // planner writing straight into `avail/` is likely to carry, and it would
    // otherwise reach the client unconverted.
    //
    // Omitted rather than converted when it does not parse. Converting would make
    // the wire look correct while leaving the body still broken for
    // `recycle_lease`, which rebuilds the `avail/` filename from this same field
    // and strands the job in `leased/` when it fails to parse (`docs/planner.md`).
    // The field is optional, so "no stated expiry" is both true and safer for a
    // client to believe than a deadline the server may not be scheduling on.
    match field("expires_at") {
        Some(serde_json::Value::String(s)) => match s.parse::<ExpiresAt>() {
            Ok(ExpiresAt::At(_)) => {
                envelope.insert("expires_at".to_string(), serde_json::Value::String(s));
            }
            // `never` is the stored spelling of "no expiry", not a timestamp; the
            // wire field is optional, so omit it rather than making every client
            // special-case the literal.
            Ok(ExpiresAt::Never) => {}
            Err(_) => tracing::warn!(
                job_id = ?envelope.get("job_id"),
                expires_at = %s,
                "omitting an unparseable `expires_at` from the claim response — the wire \
                 contract is ISO 8601 basic format (20240908T000000Z). This body will also \
                 fail the `avail/` filename rebuild when its lease lapses, stranding the \
                 job in leased/; fix the body at its source"
            ),
        },
        Some(v) if !v.is_null() => tracing::warn!(
            job_id = ?envelope.get("job_id"),
            "omitting a non-string `expires_at` from the claim response"
        ),
        _ => {}
    }
    if let Some(spec) = spec {
        envelope.insert("spec".to_string(), spec);
    }
    serde_json::Value::Object(envelope)
}

// POST /plans/claim
pub async fn claim(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
) -> Result<Response, AppError> {
    // Approved clients only — pending clients get 403 (httpapi.md §2.9.4).
    if client.status != ClientStatus::Approved {
        return Err(AppError::Forbidden("client is not approved".into()));
    }
    let client_id = &client.client_id;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();

    // 1. Suspension check — a suspended client never gets a job (204).
    if todo
        .read_suspension(client_id)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 2. Reindex gate. A pending-reindex flag means this client's eligible
    //    markers were computed from a device profile it no longer has (or, for
    //    a freshly registered client, not computed at all), so its eligibility
    //    is unknown — handing out work against stale markers could assign a
    //    job the client no longer matches. No work until `queue-maintenance`
    //    re-evaluates the client and clears the flag: at most one cron
    //    interval of latency. Re-checked after a successful claim — this
    //    check alone cannot see a flag that rises mid-request (see
    //    `revert_claim_if_pending_reindex`).
    if todo
        .has_pending_reindex(client_id)
        .await
        .map_err(internal)?
    {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 3. Existing-lease check. Scan leased/ once (across all client partitions).
    //    Collect this client's own live leases and, separately, the job_ids of
    //    every *other* client's live lease (the `taken` set) so step 5 can skip
    //    already-claimed jobs.
    //
    //    `own_live` is keyed by `job_id` so that N keys for one job collapse to
    //    one entry: a lease renewal renames `{job}_{old}` → `{job}_{new}`, and a
    //    non-snapshot paginated listing can surface both keys in one pass. They
    //    are one logical lease; keeping the later expiry, the count below then
    //    reflects distinct held jobs — otherwise a client heartbeating a single
    //    job could read as an accumulation and be falsely suspended.
    let leased = todo.list_leased().await.map_err(internal)?;
    let mut taken: HashSet<JobId> = HashSet::new();
    let mut own_live: HashMap<JobId, DateTime<Utc>> = HashMap::new();
    for name in &leased {
        let Ok((job_id, lease_client, lease_expiry)) = parse_leased_key(name) else {
            continue;
        };
        if lease_expiry <= now {
            // Expired lease; queue-maintenance will recycle it. Not "taken".
            continue;
        }
        if &lease_client == client_id {
            own_live
                .entry(job_id)
                .and_modify(|e| *e = (*e).max(lease_expiry))
                .or_insert(lease_expiry);
        } else {
            taken.insert(job_id);
        }
    }

    // A client holds at most one lease at a time, so its live-lease count
    // decides what claiming-while-leased means:
    //
    // - More than one → a protocol anomaly (a fast-rebooting client that
    //   accumulated leases across crashes). Suspend it, recording one held
    //   job_id as an operator triage breadcrumb, and return 204.
    // - Exactly one → the client is re-polling while already holding a job. The
    //   innocent cause is a claim whose response was lost in transit: the client
    //   never learned the job_id, so it cannot reclaim. Hand the same job back
    //   idempotently — return its body as a fresh claim would, but with
    //   `time_window` set to the lease's *remaining* life and without renewing
    //   the lease. Leaving the expiry untouched preserves the recycle safety
    //   valve: a genuinely crash-looping client keeps reacquiring the job but
    //   never extends it, so the lease still lapses on schedule.
    //
    // See planner.md, "Existing lease check".
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
                .map_err(internal)?;
        }
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if let Some((job_id, lease_expiry)) = own_live.iter().next() {
        // `get_leased` returns `None` when no lease exists at the key we listed:
        // it was recycled or completed, or a concurrent renewal (a zombie
        // heartbeat/reclaim from a duplicate process) renamed it to a later
        // expiry. Either way there is nothing to hand back at this key, so fall
        // through to normal selection as an ordinary claimer.
        if let Some(body) = todo
            .get_leased(job_id, client_id, *lease_expiry)
            .await
            .map_err(internal)?
        {
            // A device-profile PATCH can raise the pending-reindex flag between
            // step 2's gate and here, relinquishing this lease concurrently.
            // Re-check and return 204 rather than hand back a job the profile
            // change just voided — a submission for it would be forfeited anyway.
            // Unlike the fresh-claim path this needs no lease revert: the lease
            // already exists in `leased/`, so the relinquish's listing sees and
            // resolves it (see `revert_claim_if_pending_reindex` for the side
            // that a freshly renamed, not-yet-visible lease requires instead).
            if todo
                .has_pending_reindex(client_id)
                .await
                .map_err(internal)?
            {
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
            let remaining_secs = (*lease_expiry - now).num_seconds().max(0) as u64;
            let body = claim_response(&body, remaining_secs);
            tracing::info!(
                client_id = %client_id,
                job_id = %job_id,
                "idempotent claim: returned already-held lease to re-polling client"
            );
            return Ok(Json(body).into_response());
        }
    }

    // 4. Eligible candidates for this client, each with its `expires_at` read
    //    straight from the marker filename. The store returns these in no
    //    particular order; step 5 imposes the selection order.
    let candidates = todo
        .list_eligible_for_client(client_id)
        .await
        .map_err(internal)?;
    if candidates.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 5. Rank candidates by claim preference: soonest-expiring first, with a
    //    random tiebreak within an expiry tier. Jobs another client already
    //    holds a live lease on (step 3's `taken` set) are dropped here. A
    //    planner run stamps every job it creates with the same expiry, so the
    //    soonest tier is usually large; randomizing within it spreads load
    //    across the tier instead of having every client stampede the same job.
    //    The result is an order, not a single pick: the loop below tries each in
    //    turn, so a lost claim race (or a marker whose job is already gone)
    //    falls through to the next candidate in the tier, then to the next tier.
    let mut ranked: Vec<(JobId, ExpiresAt, u64)> = candidates
        .into_iter()
        // Drop jobs another client already holds (step 3's `taken` set) and jobs
        // already past their `expires_at` — an expired job is never handed out,
        // even if `queue-maintenance` has not yet swept it from avail/ (see
        // planner.md §Expiration). Without this, the soonest-expiring-first sort
        // below would actively *prefer* an expired candidate.
        .filter(|(job_id, exp)| !taken.contains(job_id) && !exp.is_expired(now))
        .map(|(job_id, exp)| (job_id, exp, OsRng.next_u64()))
        .collect();
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));

    let lease_secs = state.config.plan_lease_duration_secs.get();
    let lease_expiry = state.config.lease_expiry_from(now);

    for (job_id, expires_at, _) in &ranked {
        // Skip jobs this client has already reported a failure for.
        if todo
            .list_denied_for_job(job_id)
            .await
            .map_err(internal)?
            .contains(client_id)
        {
            continue;
        }
        match todo
            .claim_job(job_id, *expires_at, client_id, lease_expiry)
            .await
            .map_err(internal)?
        {
            ClaimResult::Claimed(body) => {
                if revert_claim_if_pending_reindex(
                    todo,
                    &*state.submission_store,
                    client_id,
                    job_id,
                    lease_expiry,
                )
                .await?
                {
                    return Ok(StatusCode::NO_CONTENT.into_response());
                }
                // The fresh lease grants the full server-configured increment.
                return Ok(Json(claim_response(&body, lease_secs)).into_response());
            }
            // Lost the race — another claimer took it between LIST and rename.
            ClaimResult::Gone => continue,
        }
    }

    // 6. Nothing claimable.
    Ok(StatusCode::NO_CONTENT.into_response())
}

// PUT /plans/{job_id}/heartbeat
pub async fn heartbeat(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    Path(job_id): Path<String>,
) -> Result<Response, AppError> {
    // Approved clients only — pending clients get 403 (httpapi.md §2.10.4).
    if client.status != ClientStatus::Approved {
        return Err(AppError::Forbidden("client is not approved".into()));
    }
    let client_id = &client.client_id;
    let job_id = JobId::try_new(job_id)?;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();

    let new_expiry = state.config.lease_expiry_from(now);

    // Reindex gate. A pending-reindex flag voids the client's standing in the
    // queue outright: the profile change that set it relinquished every lease
    // the client held, so a heartbeat arriving while the flag is up is the
    // client renewing a lease it has already given up — a protocol violation
    // (clients update their profile at startup and do nothing else until
    // done). Refuse *without* renaming the lease, so the renewal cannot
    // resurrect it out from under the relinquish; 404's documented flow
    // (reclaim, then abort and re-poll) routes the client back through the
    // gated endpoints until `queue-maintenance` re-evaluates it.
    if todo
        .has_pending_reindex(client_id)
        .await
        .map_err(internal)?
    {
        tracing::warn!(
            client_id = %client_id,
            job_id = %job_id,
            reason = "pending_reindex",
            "protocol violation: heartbeat during pending reindex — a broken client is attempting to renew a lease its own profile update relinquished"
        );
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    // The store locates this client's current lease for `job_id` itself (a
    // targeted `leased/{client_id}/` prefix scan) and renames it to the new
    // expiry. A missing lease is the expected reaped-lease path, not an error:
    // `renew_lease` reports `NotFound` (recycled → 404) vs `WrongClient`
    // (re-claimed by another client → 409) without the handler ever addressing
    // a key it doesn't hold.
    match todo
        .renew_lease(&job_id, client_id, new_expiry)
        .await
        .map_err(internal)?
    {
        // 200 with an empty body (httpapi.md §2.10.3).
        RenewLeaseResult::Renewed => Ok(StatusCode::OK.into_response()),
        // Lease expired and was recycled; client should try reclaim, then re-poll.
        RenewLeaseResult::NotFound => Ok(StatusCode::NOT_FOUND.into_response()),
        // Job is leased to a different client; the caller is a zombie.
        RenewLeaseResult::WrongClient => Ok(StatusCode::CONFLICT.into_response()),
    }
}

// POST /plans/{job_id}/reclaim
//
// Re-acquire the lease on a job the client was already running, after its lease
// expired during a network outage. Composes two existing primitives: first try
// to renew an existing self-lease (the in-progress path), then fall back to
// re-acquiring the job from avail/ (the same atomic rename `claim` uses). See
// planner.md §"The Client/Management Interaction" and httpapi.md §2.11.
pub async fn reclaim(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    Path(job_id): Path<String>,
) -> Result<Response, AppError> {
    // Approved clients only — pending clients get 403 (httpapi.md §2.11.4).
    if client.status != ClientStatus::Approved {
        return Err(AppError::Forbidden("client is not approved".into()));
    }
    let client_id = &client.client_id;
    let job_id = JobId::try_new(job_id)?;
    let todo = state.todo_store.as_ref();
    let now = Utc::now();

    // Suspended clients cannot reclaim → 403 (httpapi.md §2.11.4). Unlike `claim`
    // (which hides suspension behind a 204), reclaim targets a named job and
    // reports the refusal.
    if todo
        .read_suspension(client_id)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(AppError::Forbidden("client is suspended".into()));
    }

    let new_expiry = state.config.lease_expiry_from(now);

    // 1. Reindex gate. A pending-reindex flag voids the client's whole
    //    standing in the queue, so it gates the entire endpoint, renewal
    //    included — not just the marker-driven re-acquire below. The profile
    //    change that set the flag relinquished every lease the client held,
    //    so there is no lease it legitimately still holds, and its eligible
    //    markers no longer reflect its profile; a client must not run work it
    //    does not (knowably) qualify for. Refusing *before* the renewal also
    //    keeps the rename from resurrecting a lease out from under the
    //    relinquish. 404 like the no-marker case — nothing is this client's
    //    to take or resume until `queue-maintenance` re-evaluates it.
    //    Re-checked after a successful re-acquire — this check alone cannot
    //    see a flag that rises mid-request (see
    //    `revert_claim_if_pending_reindex`).
    if todo
        .has_pending_reindex(client_id)
        .await
        .map_err(internal)?
    {
        tracing::warn!(
            client_id = %client_id,
            job_id = %job_id,
            reason = "pending_reindex",
            "refused reclaim during pending reindex — the client's profile change relinquished its queue standing; prior work is forfeited"
        );
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    // 2. In-progress path. If this client still holds the lease — e.g. it expired
    //    but `queue-maintenance` has not recycled it yet — renew it and return
    //    200. Checked ahead of the eligibility/expiry gates below, so an
    //    actively-held job is never yanked from a running client by a stale
    //    eligible index, and so we pre-empt a race with the recycler. A job in
    //    progress is allowed to finish past its own `expires_at` (planner.md
    //    §Expiration), so there is deliberately no expiry check on this path.
    //    `WrongClient` means another client holds the lease → 409 (the zombie).
    match todo
        .renew_lease(&job_id, client_id, new_expiry)
        .await
        .map_err(internal)?
    {
        RenewLeaseResult::Renewed => return Ok(StatusCode::OK.into_response()),
        RenewLeaseResult::WrongClient => return Ok(StatusCode::CONFLICT.into_response()),
        // Nobody holds the lease — fall through to re-acquire from avail/.
        RenewLeaseResult::NotFound => {}
    }

    // 3. Re-acquire path. The client no longer holds the lease and no other
    //    client does. Re-acquire from avail/ under the same gates as `claim`. The
    //    eligible marker carries the job's `expires_at`, which we need to address
    //    the avail/ rename source; no marker means this client is not eligible
    //    for the job → 404.
    let Some((_, expires_at)) = todo
        .list_eligible_for_client(client_id)
        .await
        .map_err(internal)?
        .into_iter()
        .find(|(candidate, _)| candidate == &job_id)
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    // An expired job is indistinguishable from one that no longer exists → 404
    // (planner.md §Expiration).
    if expires_at.is_expired(now) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    // This client already reported a retriable failure for the job → 404.
    if todo
        .list_denied_for_job(&job_id)
        .await
        .map_err(internal)?
        .contains(client_id)
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    // 4. Atomic re-acquire. On success the response body is empty (httpapi.md
    //    §2.11.3) — the client still has the job JSON from its original claim, so
    //    the reclaimed body is discarded. `Gone` means the job left avail/
    //    between the checks above and the rename (completed, expired-and-swept,
    //    or claimed by another in the inherent race) → 404.
    match todo
        .claim_job(&job_id, expires_at, client_id, new_expiry)
        .await
        .map_err(internal)?
    {
        ClaimResult::Claimed(_) => {
            if revert_claim_if_pending_reindex(
                todo,
                &*state.submission_store,
                client_id,
                &job_id,
                new_expiry,
            )
            .await?
            {
                return Ok(StatusCode::NOT_FOUND.into_response());
            }
            Ok(StatusCode::OK.into_response())
        }
        ClaimResult::Gone => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::stores::{LocalFsSubmissionStore, build_local_fs_todo_store};
    use crate::todo_filename::leased_key;
    use rstest::rstest;

    /// The post-claim recheck reverts a fresh lease exactly when the
    /// pending-reindex flag is up: the racing relinquish cannot see a lease
    /// created after its re-list (see [`revert_claim_if_pending_reindex`]),
    /// so the claim path itself must return the job to `avail/`. With no
    /// flag, the lease stands untouched.
    #[rstest]
    #[case::flag_up_reverts(true)]
    #[case::no_flag_keeps_lease(false)]
    #[tokio::test]
    async fn revert_claim_only_when_pending_reindex(#[case] flagged: bool) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        };
        let todo = build_local_fs_todo_store(&config)?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));

        // A lease as `claim_job` leaves it, seeded directly: the recheck runs
        // strictly after the `avail/ → leased/` rename.
        let job = JobId::new_unchecked("job-revert");
        let client = ClientId::try_new("revert-client")?;
        let lease_expiry: DateTime<Utc> = "2027-01-01T00:00:00Z".parse()?;
        let path =
            dir.path()
                .join("todo")
                .join("leased")
                .join(leased_key(&job, &client, lease_expiry));
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("leased key has no parent dir"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "job_id": "job-revert" }))?,
        )?;
        if flagged {
            todo.write_pending_reindex(&client).await?;
        }

        let reverted =
            revert_claim_if_pending_reindex(&*todo, &submissions, &client, &job, lease_expiry)
                .await?;

        assert_eq!(reverted, flagged);
        // Reverted: the lease is gone and the job is claimable again.
        // Untouched: the lease stands and nothing was recycled.
        assert_eq!(todo.get_avail_by_job(&job).await?.is_some(), flagged);
        assert_eq!(todo.list_leased().await?.is_empty(), flagged);
        Ok(())
    }
}
