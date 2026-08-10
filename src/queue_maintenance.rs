//! The `pipette-mgmt queue-maintenance` command: all cron reconciliation for
//! the `todo/` job queue. See `docs/planner.md` (§Processes, §Client Matching
//! Rules, and the eligible-index design) and `docs/operations.md` §3.1–3.2 for
//! the cron setup.
//!
//! [`run`] executes the passes in a deliberate order:
//!
//! 1. **Expired leases** are recycled `leased/ → avail/`, so a job whose
//!    holder went silent is claimable again — and is present in `avail/` for
//!    every listing below. A lease whose job already has a submission record
//!    is a teardown leftover, not a silent holder: it is deleted instead,
//!    since recycling it would make the finished job claimable again.
//! 2. **Expired jobs** (`avail/` entries past their `expires_at`) are
//!    converted to synthetic `"system"` failure records and deleted, so the
//!    index passes never see them and the GC sweeps collect their markers in
//!    the same run. A lease that expired past its job's deadline is recycled
//!    by pass 1 and expired here, in one run.
//! 3. **All-denied jobs** (`avail/` entries whose closed `clients` roster is
//!    fully denied) are escalated to synthetic `"system"` failures the same
//!    way. This pass is the sole owner of the all-denied rule — the submit
//!    path only records the denial, so an exhausted roster waits at most one
//!    cron interval for escalation here.
//! 4. **Eligible index** — new `avail/` arrivals are indexed against all
//!    clients (cursor-paged), and clients flagged by a device-profile change
//!    are re-evaluated against the whole queue. Jobs that were leased during
//!    such a re-evaluation (invisible to its `avail/`-sourced rebuild) are
//!    flagged into `pending-reindex-jobs/` and settled — re-matched against
//!    every client — once back in `avail/`.
//! 5. **Orphan reconciliation** walks every per-entity marker tree
//!    (`eligible/`, `denied/`, `suspended/`, `pending-reindex/`,
//!    `pending-reindex-jobs/`, and `leased/`) and reconciles each against the
//!    run's source of truth: a job
//!    is live iff it is in `avail/ ∪ leased/`, a client iff it is in the
//!    roster (`auth.list_clients()`). Any marker whose owner no longer exists
//!    — however it was orphaned (a job terminating, a client deleted, a failed
//!    `clients delete`, a lost race) — is collected. Collection is confirmed:
//!    by this run's stale-lease, expiry, or escalation resolution, or by the
//!    marker's storage key being seen orphaned in two consecutive runs (a
//!    persisted candidate set carries first sightings across runs), so an
//!    entity racing the listings never loses live markers. A lease held by a
//!    deleted client is *recycled* (`leased/ → avail/`), not deleted, so the
//!    job body is preserved and the job becomes claimable again.
//! 6. **Stale `tmp/`** partial job files from a crashed planner are deleted.
//!
//! Passes 1–3 are lenient per item: a failed lease or job is logged and
//! skipped so it cannot wedge the rest of the run (the next cron tick retries
//! it), but any such failure makes the run exit non-zero so a persistent one
//! surfaces through cron monitoring.
//!
//! Two deployment assumptions this module relies on for correctness (see
//! `docs/operations.md` §3.1):
//!
//! - **Runs are externally serialized** (cron `flock -n`, or a Kubernetes
//!   `concurrencyPolicy: Forbid`). The two-sighting GC rule persists first
//!   sightings for the *next* run to read; it is sound only because that next
//!   run is a full cron interval later, long enough for an in-transition job to
//!   become visible. Two overlapping runs could supply both "consecutive"
//!   sightings seconds apart and sweep a live job's markers.
//! - **The maintenance host's clock does not run ahead of the serve hosts'.**
//!   Expiry decisions here use `now`, captured once at run start, while the
//!   claim/reclaim gates use the serve host's clock. Because `now` only lags as
//!   the run proceeds, on a shared clock a job this pass considers expired is
//!   also expired for the claim gates (so none can be handed out under it); a
//!   clock running *ahead* breaks that and can expire a job a client is
//!   actively claiming.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::benchmark::Benchmark;
use crate::client::Client;
use crate::matching::{capability_clauses_malformed, job_matches_client};
use crate::stores::{AuthStore, RecycleResult, SubmissionStore, TodoStore};
use crate::submission::{UnrecordableJob, record_system_failure};
use crate::todo_filename::{eligible_filename, parse_avail_filename, parse_leased_key};
use crate::types::{BenchmarkId, ClientId, ExpiresAt, JobId};

/// `avail/` list page size.
const PAGE: usize = 100;

/// One parsed `avail/` entry: the raw key (used for cursor comparison and as the
/// body-cache key) alongside its decoded `(job_id, expires_at)`. Parsing happens
/// once, in [`collect_all_avail`], so the passes below never re-parse.
struct AvailKey {
    key: String,
    job_id: JobId,
    expires_at: ExpiresAt,
}

/// Run every queue-maintenance pass (see the module doc for the order and
/// why it matters). Safe to run repeatedly; eventually consistent within one
/// cron interval. The benchmark catalog resolves `benchmark_type` for the
/// expiry pass's synthetic failures.
pub async fn run(
    todo: &dyn TodoStore,
    auth: &dyn AuthStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    tmp_max_age: Duration,
) -> anyhow::Result<()> {
    let now = Utc::now();
    // The candidate set holds the previous run's orphan first sightings, and a
    // marker deletion is sound only when its two sightings come from
    // *consecutive* completed runs (see the module doc). Consume the set
    // exactly once — read it, then immediately clear it — so a run that fails
    // partway cannot leave sightings behind for a later run to mistake as the
    // previous run's. A failed run thus degrades to "no history next run":
    // GC delayed one interval, the safe direction. This run's fresh sightings
    // are recorded at the end of the run.
    let prior_gc_candidates = todo.read_gc_candidates().await?;
    todo.write_gc_candidates(&HashSet::new()).await?;

    let (leased_at_recycle, resolved_at_recycle, failed_items) =
        recycle_expired_leases(todo, submissions, now).await?;

    // One `avail/` listing feeds every pass below: taken after the recycle
    // pass so recycled jobs are included, then filtered by the expiry pass to
    // the entries that remain in `avail/`. `leased/` is listed immediately
    // after `avail/`, and every lease the recycle pass saw also counts as
    // live, so a concurrent `leased/ → avail/` recycle escapes the GC live
    // set only when the claim *and* the recycle both land inside this run —
    // after the recycle pass's `leased/` listing and straddling the `avail/`
    // pager's visit to the job's key. Even then the job keeps its markers:
    // the GC sweeps delete only on the second consecutive orphaned sighting
    // (see the candidate set below), and a job that raced one run's listings
    // is visibly live by the next run. That two-sighting rule matters because
    // a wrongly swept marker would never be rebuilt (the job's key is behind
    // the eligible-index cursor), leaving an `ExpiresAt::Never` job silently
    // unclaimable forever.
    let avail_keys = collect_all_avail(todo).await?;
    // Parsed once, this snapshot feeds both the GC live set and the
    // deferred-reindex flags in `reindex_flagged_clients`. Unparseable keys
    // are foreign cruft (see `recycle_expired_leases`) and already warned
    // there this run.
    let leased_ids: HashSet<JobId> = todo
        .list_leased()
        .await?
        .iter()
        .filter_map(|k| parse_leased_key(k).ok())
        .map(|(job_id, _, _)| job_id)
        .collect();

    let (all_keys, expired_now, expiry_failures) =
        expire_overdue_jobs(todo, submissions, catalog, avail_keys, now).await?;

    // Bodies fetched by the escalation pass are reused by the index passes.
    let mut bodies: HashMap<String, Value> = HashMap::new();
    let (all_keys, escalated_now, escalation_failures) =
        escalate_all_denied_jobs(todo, submissions, catalog, all_keys, &mut bodies, now).await?;
    let failed_items = failed_items + expiry_failures + escalation_failures;

    // Every positively-removed job — a stale lease resolved against an
    // existing record, expired, or escalated — is confirmed terminal for this
    // run's GC sweeps.
    let mut terminal_now = expired_now;
    terminal_now.extend(escalated_now);
    terminal_now.extend(resolved_at_recycle);

    let clients = auth.list_clients().await?;
    // Live set for the GC sweeps: a job is permanently removed only when it is
    // in neither `avail/` nor `leased/` (planner.md). A *leased* job's markers
    // must survive — its `avail/` key sorts behind the eligible-index cursor,
    // so markers dropped during the lease would never be rebuilt and a
    // recycled lease would leave the job permanently unclaimable (even
    // `reclaim` reads the marker).
    //
    // `terminal_now` (the jobs this run's recycle, expiry, and escalation
    // passes positively removed or resolved) is subtracted from the
    // `leased_at_recycle` contribution only: a job whose expired lease was
    // recycled by pass 1 and then expired by pass 2 — or whose stale lease
    // pass 1 deleted against an existing record — is still in
    // `leased_at_recycle`, yet it is terminal — excluding it is what lets its
    // markers be swept this same run instead of lingering until the
    // two-sighting orphan path catches them.
    //
    // The other two contributions are deliberately not filtered: an entry
    // still in `avail/` is claimable, and a lease in this run's fresh
    // `leased/` listing is held by a client right now, so both are live even
    // if a pass removed *another* entry for the same job id (a duplicate-entry
    // state only a planner bug produces, but one this sweep must not turn
    // destructive). In an intact queue neither can intersect `terminal_now`,
    // so the filter's absence costs nothing.
    let live_ids: HashSet<JobId> = all_keys
        .iter()
        .map(|a| a.job_id.clone())
        .chain(leased_ids.iter().cloned())
        .chain(
            leased_at_recycle
                .into_iter()
                .filter(|id| !terminal_now.contains(id)),
        )
        .collect();
    tracing::info!(
        avail_jobs = all_keys.len(),
        live_jobs = live_ids.len(),
        clients = clients.len(),
        "queue-maintenance: eligible-index pass starting"
    );

    index_new_jobs(todo, &clients, &all_keys, &mut bodies).await?;
    reindex_flagged_clients(todo, auth, &all_keys, &leased_ids, &mut bodies).await?;
    settle_pending_reindex_jobs(todo, auth, submissions, &all_keys, &mut bodies).await?;

    // Reconcile every per-entity marker tree against this run's source of
    // truth — the live-job set above and the live-client roster (`clients`,
    // the same snapshot the index passes used). The sweep runs after those
    // passes so it never deletes a marker one of them just wrote, and acts on
    // a marker only when it is *confirmed* orphaned: its job was positively
    // removed this run (`terminal_now`, same-run) or its storage key was seen
    // orphaned by the previous run (`prior_gc_candidates`, consumed at the top
    // of the run). A first sighting is only re-staged — an entity that bounced
    // back between runs is visibly live by the next one and drops off.
    let live_clients: HashSet<ClientId> = clients.iter().map(|c| c.client_id.clone()).collect();
    let next_candidates = reconcile_orphans(
        todo,
        submissions,
        &live_ids,
        &live_clients,
        &terminal_now,
        &prior_gc_candidates,
    )
    .await?;
    todo.write_gc_candidates(&next_candidates).await?;

    cleanup_stale_tmp(todo, tmp_max_age).await?;

    if failed_items > 0 {
        anyhow::bail!(
            "queue-maintenance: {failed_items} item(s) failed and were skipped (see warnings); \
             they will be retried on the next run"
        );
    }
    Ok(())
}

/// Recycle every lease whose `lease_expiry` is in the past back to `avail/`,
/// making the job claimable again — unless the job already has a submission
/// record. The submit path's terminal teardown is best-effort (and a
/// concurrent heartbeat can rename the lease out from under its delete), so a
/// lease can outlive a job whose real result is already persisted; recycling
/// it would make the finished job claimable again, and the re-run's result
/// would land at the same `processed/` key as the existing record. Such a
/// stale lease is instead deleted, finishing the teardown. `Gone` (the
/// holder's terminal submission or another actor resolved the lease between
/// the LIST and the rename) is a benign skip; a real failure — including a
/// failed record check, where recycling anyway would be the destructive
/// guess — is logged and skipped, and counts toward the run's non-zero exit.
///
/// Returns, in order:
/// - the `JobId` of every parseable lease the pass listed — expired or not.
///   The caller counts those jobs as live for the GC sweeps: any of them can
///   be recycled `leased/ → avail/` by a concurrent actor while the later
///   listings run, and a recycled job's markers must survive (its `avail/`
///   key is behind the eligible-index cursor, so swept markers would never be
///   rebuilt);
/// - the jobs positively resolved here (stale lease deleted against an
///   existing record), which the caller confirms terminal for the same run's
///   GC sweeps;
/// - the failed-item count for the run's non-zero exit.
async fn recycle_expired_leases(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    now: DateTime<Utc>,
) -> anyhow::Result<(HashSet<JobId>, HashSet<JobId>, usize)> {
    let mut seen = HashSet::new();
    let mut resolved = HashSet::new();
    let mut recycled = 0usize;
    let mut failures = 0usize;
    for key in todo.list_leased().await? {
        // An unparseable leased key is not a system-created lease: every lease
        // this system writes parses (enforced at construction, covered by
        // tests), so an entry that does not is foreign cruft — an operator's
        // stray file, a `.DS_Store`, and the like. It corresponds to no real
        // job, so it is deliberately left in place (never deleted), excluded
        // from the GC live set, and not counted as a failure — omitting a
        // non-job cannot falsely orphan a real one. Logged once here so its
        // presence is visible without wedging the run.
        let Ok((job_id, client_id, lease_expiry)) = parse_leased_key(&key) else {
            tracing::warn!(key = %key, "skipping unparseable leased key");
            continue;
        };
        seen.insert(job_id.clone());
        if lease_expiry >= now {
            continue;
        }
        // On a failed resolution the lease is left for the next run: skipping
        // delays a legitimate recycle by one cron interval, while acting on a
        // wrong guess can re-run a finished job and clobber its record.
        match resolve_or_recycle_lease(todo, submissions, &job_id, &client_id, lease_expiry).await {
            Ok(LeaseResolution::Recycled) => {
                tracing::info!(
                    job_id = %job_id,
                    client_id = %client_id,
                    lease_expiry = %lease_expiry,
                    "queue-maintenance: recycled expired lease to avail/"
                );
                recycled += 1;
            }
            Ok(LeaseResolution::ResolvedStale) => {
                tracing::info!(
                    job_id = %job_id,
                    client_id = %client_id,
                    lease_expiry = %lease_expiry,
                    "queue-maintenance: job already has a submission record; deleted stale lease"
                );
                resolved.insert(job_id.clone());
            }
            Ok(LeaseResolution::Gone) => {
                tracing::debug!(job_id = %job_id, client_id = %client_id, "expired lease already resolved elsewhere");
            }
            Err(e) => {
                tracing::warn!(job_id = %job_id, client_id = %client_id, error = %e, "failed to resolve expired lease");
                failures += 1;
            }
        }
    }
    if recycled > 0 {
        tracing::info!(
            recycled,
            "queue-maintenance: expired-lease recycle pass done"
        );
    }
    Ok((seen, resolved, failures))
}

/// Outcome of [`resolve_or_recycle_lease`] for one lease.
#[derive(Debug)]
pub(crate) enum LeaseResolution {
    /// The job has no submission record; the lease was renamed back to
    /// `avail/` and the job is claimable again.
    Recycled,
    /// The job already has a submission record, so the lease was a teardown
    /// leftover: deleted, job confirmed terminal. Callers must not treat the
    /// job as claimable.
    ResolvedStale,
    /// The lease vanished between the caller's listing and this call —
    /// another actor (the holder's terminal submission, a concurrent
    /// maintenance run) resolved it. Benign.
    Gone,
}

/// Resolve one lease that should no longer be held: recycle its job back to
/// `avail/`, unless the job already has a submission record — then the lease
/// is a leftover of the submit path's best-effort teardown, and recycling it
/// would make a finished job claimable again (its re-run's result would land
/// at the existing record's `processed/` key). The record check (`find_job`
/// covers `incoming/`, `processed/`, and the score-queue) therefore decides:
/// record → delete the lease, finishing the teardown; no record → recycle.
///
/// Shared by the queue-maintenance expired-lease pass and the profile-change
/// relinquish in `PATCH /clients/me`. Any step failing propagates — the
/// caller decides whether that is fatal or a skip-and-retry.
pub(crate) async fn resolve_or_recycle_lease(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    job_id: &JobId,
    client_id: &ClientId,
    lease_expiry: DateTime<Utc>,
) -> anyhow::Result<LeaseResolution> {
    if submissions.find_job(job_id).await?.is_some() {
        todo.delete_lease(job_id, client_id, lease_expiry).await?;
        return Ok(LeaseResolution::ResolvedStale);
    }
    match todo.recycle_lease(job_id, client_id, lease_expiry).await? {
        RecycleResult::Recycled => Ok(LeaseResolution::Recycled),
        RecycleResult::Gone => Ok(LeaseResolution::Gone),
    }
}

/// Convert every job in `all_keys` past its `expires_at` into a terminal
/// synthetic `"system"` failure and tear it down (see [`expire_one_job`]).
/// Returns, in order:
/// - the entries still present in `avail/` after the pass — the not-expired
///   jobs plus any whose expiry failed (logged, skipped, and retried next run)
///   — preserving `all_keys`'s key order, which the eligible-index cursor in
///   [`index_new_jobs`] depends on;
/// - the set of job ids this pass *positively removed* (a record was written or
///   already existed), which the caller confirms terminal for the same run's GC
///   sweeps. An entry that merely vanished mid-pass is excluded: its origin is
///   ambiguous, so its markers are left to the two-sighting orphan path;
/// - the failed-item count for the run's non-zero exit.
async fn expire_overdue_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailKey>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailKey>, HashSet<JobId>, usize)> {
    let mut survivors = Vec::with_capacity(all_keys.len());
    let mut removed = HashSet::new();
    let mut failures = 0usize;
    for avail in all_keys {
        // `ExpiresAt::is_expired` owns the expiry rule (the claim/reclaim
        // gates use the same predicate); the `At` pattern only binds the
        // deadline for the failure-reason string.
        let deadline = match avail.expires_at {
            ExpiresAt::At(deadline) if avail.expires_at.is_expired(now) => deadline,
            _ => {
                survivors.push(avail);
                continue;
            }
        };
        match expire_one_job(todo, submissions, catalog, &avail, deadline, now).await {
            // Positively removed by this pass (a synthetic failure was written,
            // or a real record already existed and the leftover `avail/` entry
            // was cleared): confirmed terminal, so its markers may be swept in
            // this same run.
            Ok(true) => {
                removed.insert(avail.job_id.clone());
            }
            // The entry vanished before we could act (e.g. planner-deleted).
            // Dropping it from the survivors is safe — `claim`/`reclaim` refuse
            // expired entries — but its origin is ambiguous, so it is not
            // confirmed terminal here; the two-sighting orphan path sweeps its
            // markers once it is seen orphaned in two consecutive runs.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(job_id = %avail.job_id, error = %e, "failed to expire overdue job");
                failures += 1;
                survivors.push(avail);
            }
        }
    }
    Ok((survivors, removed, failures))
}

/// Escalate every `avail/` job whose closed `clients` roster is fully denied —
/// no listed client can ever succeed it, so it is converted to a terminal
/// synthetic `"system"` failure and its `avail/` entry deleted. This pass is
/// the sole owner of the all-denied rule: the submit path only records the
/// denial (`write_denied`) and recycles the job, so an `ExpiresAt::Never` job
/// whose roster is exhausted would otherwise sit unclaimable forever — every
/// listed client holds a `denied/` marker, and `claim` skips denied
/// candidates.
///
/// Driven from the `denied/` listing, not the `avail/` listing: only a job
/// with at least one denial can be all-denied, so job bodies are fetched only
/// for that (typically tiny) candidate set instead of the whole queue. Fetched
/// bodies are parked in `bodies` for the index passes to reuse.
///
/// Returns the same shape as [`expire_overdue_jobs`]: the surviving entries in
/// key order, the job ids positively removed (confirmed terminal for the same
/// run's GC sweeps), and the failed-item count. An entry that vanished between
/// the listing and the body read is kept as a survivor — unlike the expiry
/// pass's vanished case it is claimable, so "vanished" plausibly means "just
/// claimed" and the job must stay in the live set.
///
/// A job that already has a submission record is not escalated — the same
/// teardown-leftover guard as [`expire_one_job`]: the stale `avail/` entry is
/// deleted and the record left untouched.
async fn escalate_all_denied_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailKey>,
    bodies: &mut HashMap<String, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailKey>, HashSet<JobId>, usize)> {
    let denied_by: HashMap<JobId, HashSet<String>> = todo
        .list_all_denied()
        .await?
        .into_iter()
        .fold(HashMap::new(), |mut acc, (job_id, client_id)| {
            acc.entry(job_id)
                .or_default()
                .insert(client_id.as_str().to_owned());
            acc
        });

    let mut survivors = Vec::with_capacity(all_keys.len());
    let mut removed = HashSet::new();
    let mut failures = 0usize;
    for avail in all_keys {
        let Some(denied) = denied_by.get(&avail.job_id) else {
            survivors.push(avail);
            continue;
        };
        match escalate_one_if_all_denied(todo, submissions, catalog, &avail, denied, bodies, now)
            .await
        {
            Ok(true) => {
                removed.insert(avail.job_id.clone());
            }
            Ok(false) => survivors.push(avail),
            Err(e) => {
                tracing::warn!(job_id = %avail.job_id, error = %e, "failed to escalate all-denied job");
                failures += 1;
                survivors.push(avail);
            }
        }
    }
    Ok((survivors, removed, failures))
}

/// Fetch `avail`'s job body through the run's shared body cache, populating
/// it on a miss. `Ok(None)` means the entry left `avail/` since the listing
/// (claimed, completed, or deleted) — each caller applies its own policy for
/// a vanished entry.
async fn cached_body(
    todo: &dyn TodoStore,
    avail: &AvailKey,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<Option<Value>> {
    if let Some(cached) = bodies.get(&avail.key) {
        return Ok(Some(cached.clone()));
    }
    let Some(body) = todo.get_avail(&avail.job_id, avail.expires_at).await? else {
        return Ok(None);
    };
    bodies.insert(avail.key.clone(), body.clone());
    Ok(Some(body))
}

/// The per-job body of [`escalate_all_denied_jobs`]: fetch the body, classify,
/// and escalate when the full roster is denied. Returns `true` when the job
/// was positively removed (record written, or one already existed and the
/// leftover entry was cleared); `false` when the job stays (not `clients`-only,
/// roster not exhausted, or the entry vanished — kept live by the caller).
async fn escalate_one_if_all_denied(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailKey,
    denied: &HashSet<String>,
    bodies: &mut HashMap<String, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(body) = cached_body(todo, avail, bodies).await? else {
        return Ok(false);
    };
    let all_denied = crate::matching::clients_only_roster(&body)
        .is_some_and(|roster| roster.iter().all(|c| denied.contains(*c)));
    if !all_denied {
        return Ok(false);
    }

    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        avail,
        &body,
        "All eligible clients reported failure".to_string(),
        "all_denied",
        now,
    )
    .await?;
    Ok(true)
}

/// Positively resolve an `avail/` entry as terminal — the shared tail of the
/// expiry and all-denied escalation passes.
///
/// If the job already has a submission record — terminal teardown is
/// best-effort and can leave the entry behind — only the leftover entry is
/// cleared: writing the synthetic failure would clobber or contradict the
/// real record (both land at the same `processed/` key), and `find_job` also
/// covers `incoming/` and the score-queue, so a real result still being
/// scored blocks the write too. Otherwise a synthetic `"system"` failure is
/// recorded first, then the entry deleted, so a failure between the two
/// leaves the job in `avail/` for a clean retry next run. The record lands
/// directly in `processed/` (see [`record_system_failure`]) and never flows
/// through `process-submissions`; the orphaned `eligible/`/`denied/` markers
/// are left to the GC sweeps in the same run.
///
/// `cause` labels the log events; `failure_reason` becomes the synthetic
/// record's failure message.
//
// Five of the eight parameters are the store/catalog/clock context every pass
// helper in this module threads verbatim; bundling the remaining three into a
// one-use struct would add ceremony without clarifying anything.
#[allow(clippy::too_many_arguments)]
async fn resolve_job_terminal(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailKey,
    body: &Value,
    failure_reason: String,
    cause: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    if submissions.find_job(&avail.job_id).await?.is_some() {
        todo.delete_avail(&avail.job_id, avail.expires_at).await?;
        tracing::info!(
            job_id = %avail.job_id,
            cause,
            "queue-maintenance: job already has a submission record; removed leftover avail/ entry"
        );
        return Ok(());
    }
    if let Err(e) = record_system_failure(submissions, catalog, body, failure_reason, now).await {
        // Anything but a structurally unrecordable body is worth another run: a
        // store blip, or a `benchmark_id` whose catalog definition an operator can
        // restore. Those propagate, and the caller keeps the entry.
        if !e.chain().any(|c| c.is::<UnrecordableJob>()) {
            return Err(e);
        }
        // Permanent. The body never changes, so every future run would fail here
        // identically — leaving the entry to be re-warned about forever while it
        // sits in `avail/` being handed out and lapsing. Drop it instead, naming
        // the defect. Nothing is orphaned by the missing record: a body this
        // broken cannot belong to a plan (see `UnrecordableJob`), so no manifest
        // is waiting on its outcome.
        todo.delete_avail(&avail.job_id, avail.expires_at).await?;
        tracing::warn!(
            job_id = %avail.job_id,
            expires_at = %avail.expires_at,
            cause,
            error = %e,
            "queue-maintenance: deleted a job whose body can never produce a failure \
             record — no retry could succeed; ingestion refuses such a body, so it was \
             written straight into avail/"
        );
        return Ok(());
    }
    todo.delete_avail(&avail.job_id, avail.expires_at).await?;
    tracing::info!(
        job_id = %avail.job_id,
        expires_at = %avail.expires_at,
        cause,
        "queue-maintenance: wrote synthetic system failure and removed job"
    );
    Ok(())
}

/// Expire a single overdue job, resolving it terminal via
/// [`resolve_job_terminal`] (which guards against clobbering an existing
/// record and leaves the orphaned markers to the GC sweeps).
///
/// No result can land after the guard's check: an expired entry is
/// unclaimable (`claim`/`reclaim` gate on `ExpiresAt::is_expired`) and a
/// submission requires a live lease, which a job sitting in `avail/` does not
/// have.
///
/// Returns `true` when the job was positively removed by this call (a record
/// was written, or one already existed and the leftover entry was cleared) and
/// so is confirmed terminal; `false` when the entry had already vanished, whose
/// removal this call did not cause and cannot vouch for.
async fn expire_one_job(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailKey,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(body) = todo.get_avail(&avail.job_id, avail.expires_at).await? else {
        // Vanished between the LIST and the read (planner-deleted) — nothing
        // left to expire.
        return Ok(false);
    };
    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        avail,
        &body,
        format!(
            "Job expired at {} before any client completed it",
            deadline.to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
        "expired",
        now,
    )
    .await?;
    Ok(true)
}

/// List every `avail/` key in key order (UUIDv7 → arrival order), decoding each
/// once. An unparseable filename can't be indexed: it is logged and dropped here
/// (treated as absent), so no downstream pass re-parses or re-warns about it.
async fn collect_all_avail(todo: &dyn TodoStore) -> anyhow::Result<Vec<AvailKey>> {
    let limit = NonZeroUsize::new(PAGE).expect("PAGE is nonzero");
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = todo.list_avail(cursor.as_deref(), limit).await?;
        let Some(last) = page.last().cloned() else {
            break;
        };
        cursor = Some(last);
        all.extend(
            page.into_iter()
                .filter_map(|key| match parse_avail_filename(&key) {
                    Ok((job_id, expires_at)) => Some(AvailKey {
                        key,
                        job_id,
                        expires_at,
                    }),
                    Err(_) => {
                        tracing::warn!(key = %key, "skipping unparseable avail filename");
                        None
                    }
                }),
        );
    }
    Ok(all)
}

/// Index `avail/` keys that arrived since the persisted cursor against every
/// client, writing eligible markers for matches. Advances the cursor to the
/// high-water mark so a later run skips these keys — the cursor exists precisely
/// to avoid re-fetching every job body each run (the dominant cost).
async fn index_new_jobs(
    todo: &dyn TodoStore,
    clients: &[Client],
    all_keys: &[AvailKey],
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<()> {
    let cursor = todo.read_eligible_cursor().await?;
    let new_keys: Vec<&AvailKey> = match &cursor {
        Some(c) => all_keys
            .iter()
            .filter(|a| a.key.as_str() > c.as_str())
            .collect(),
        None => all_keys.iter().collect(),
    };
    if new_keys.is_empty() {
        return Ok(());
    }

    let mut markers = 0usize;
    for avail in &new_keys {
        let Some(body) = todo.get_avail(&avail.job_id, avail.expires_at).await? else {
            continue;
        };
        if capability_clauses_malformed(&body) {
            tracing::warn!(
                job_id = %avail.job_id,
                "avail job has malformed requires/any_of; capability path matches nobody"
            );
        }
        for client in clients {
            if job_matches_client(&body, client) {
                todo.write_eligible(&client.client_id, &avail.job_id, avail.expires_at)
                    .await?;
                markers += 1;
            }
        }
        bodies.insert(avail.key.clone(), body);
    }

    // Advance to the greatest existing key (keys are in order).
    if let Some(last) = all_keys.last() {
        todo.write_eligible_cursor(&last.key).await?;
    }
    tracing::info!(
        new_jobs = new_keys.len(),
        markers_written = markers,
        "queue-maintenance: indexed new avail jobs"
    );
    Ok(())
}

/// Re-evaluate each flagged client against the **whole** `avail/` set (its
/// profile changed, so the incremental cursor pass would miss pre-existing
/// jobs). A flag for a client that no longer exists triggers marker cleanup
/// instead.
///
/// Each rebuild reads the client's record **fresh** (not the run-start
/// snapshot) and afterwards deletes **exactly the flag keys captured
/// before it** — the two halves of one guarantee: a rebuild never consumes
/// a reindex request newer than the record it evaluated. A profile change
/// landing mid-run writes a flag key outside the capture, which survives
/// this run and re-triggers on the next; a record persisted mid-run is
/// covered by the flag its `PATCH` writes *after* persisting (see
/// `update_me`).
///
/// The rebuild reads `avail/`, so a job that is currently leased — or that
/// leaves `avail/` mid-pass — cannot be evaluated here. Each such job is
/// flagged into `pending-reindex-jobs/` for [`settle_pending_reindex_jobs`]
/// to re-match against every client once the job is back in `avail/`. The
/// flags are written *before* the partition wipe: a crash between the two
/// leaves a flag with no wipe (harmless — settling is idempotent), never a
/// wipe with no flag.
async fn reindex_flagged_clients(
    todo: &dyn TodoStore,
    auth: &dyn AuthStore,
    all_keys: &[AvailKey],
    leased_ids: &HashSet<JobId>,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<()> {
    let flags = todo.list_pending_reindex().await?;
    if flags.is_empty() {
        return Ok(());
    }
    // One client may have several outstanding flag keys (each write mints a
    // new one); a single rebuild consumes them all.
    let by_client: BTreeMap<ClientId, Vec<String>> =
        flags.into_iter().fold(BTreeMap::new(), |mut m, (c, key)| {
            m.entry(c).or_default().push(key);
            m
        });

    for job_id in leased_ids {
        todo.write_pending_reindex_job(job_id).await?;
    }

    for (client_id, keys) in &by_client {
        let Some(client) = auth.get_client(client_id).await? else {
            // Deleted client — drop its markers and the captured flags. Only
            // the captured keys: a re-registration (same key-derived id)
            // racing this run may have flagged itself, and that request must
            // survive to the next run like any other.
            todo.delete_eligible_for_client(client_id).await?;
            for key in keys {
                todo.delete_pending_reindex(key).await?;
            }
            tracing::info!(
                client_id = %client_id,
                "queue-maintenance: cleared eligible markers for deleted client"
            );
            continue;
        };

        // Rebuild from scratch: wipe the partition, then re-add current matches.
        todo.delete_eligible_for_client(client_id).await?;
        let mut markers = 0usize;
        for avail in all_keys {
            let Some(body) = cached_body(todo, avail, bodies).await? else {
                // Left avail/ since the listing (claimed, completed, or
                // deleted) — not evaluable now; defer it like the leased jobs
                // above.
                todo.write_pending_reindex_job(&avail.job_id).await?;
                continue;
            };
            if job_matches_client(&body, &client) {
                todo.write_eligible(client_id, &avail.job_id, avail.expires_at)
                    .await?;
                markers += 1;
            }
        }
        // Consume only the captured keys — a flag written since the capture
        // names a request this rebuild may not have seen; it survives.
        for key in keys {
            todo.delete_pending_reindex(key).await?;
        }
        tracing::info!(
            client_id = %client_id,
            markers_written = markers,
            "queue-maintenance: reindexed client after device-profile change"
        );
    }
    Ok(())
}

/// Settle the deferred job reindexing recorded by [`reindex_flagged_clients`]:
/// each flagged job's markers may be stale for any client (some client's
/// profile changed while the job was un-evaluable). A flagged job back in
/// `avail/` is re-matched against **every** client — a marker written for each
/// match and deleted for each non-match, superseding whatever the pre-change
/// profiles had — and its flag cleared. A flagged job with a submission record
/// is terminal: flag cleared, markers left to the reconciliation sweep.
/// Anything else (still leased, or mid-transition) keeps its flag for a later
/// run.
///
/// A flag whose job was planner-deleted from `avail/` (terminal but with no
/// record, ever) is left untouched here — no lenient clearing rule can
/// distinguish it from a mid-transition job without re-risking the stale
/// markers this pass exists to fix. The reconciliation sweep collects it
/// instead, once the job is seen absent from `avail/ ∪ leased/` in two
/// consecutive runs.
///
/// The client list is read **fresh**, not from the run-start snapshot. This
/// pass runs after [`reindex_flagged_clients`] has consumed its captured
/// flags, so matching against any older record could reinstate a pre-change
/// profile's markers when the flag that would repair them is already gone —
/// wrong markers nothing would ever fix. The fresh read makes both orderings
/// converge: a profile persisted before it is matched correctly here, and one
/// persisted after it writes a flag outside the reindex pass's capture, which
/// survives to the next run's rebuild (the same discipline as
/// `reindex_flagged_clients`: never evaluate a record older than a consumed
/// reindex request).
async fn settle_pending_reindex_jobs(
    todo: &dyn TodoStore,
    auth: &dyn AuthStore,
    submissions: &dyn SubmissionStore,
    all_keys: &[AvailKey],
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<()> {
    let flags = todo.list_pending_reindex_jobs().await?;
    if flags.is_empty() {
        return Ok(());
    }
    let clients = auth.list_clients().await?;
    let avail_by_job: HashMap<&JobId, &AvailKey> =
        all_keys.iter().map(|a| (&a.job_id, a)).collect();

    for job_id in &flags {
        let Some(avail) = avail_by_job.get(job_id) else {
            if submissions.find_job(job_id).await?.is_some() {
                todo.delete_pending_reindex_job(job_id).await?;
                tracing::info!(
                    job_id = %job_id,
                    "queue-maintenance: cleared deferred-reindex flag for completed job"
                );
            }
            continue;
        };
        let Some(body) = cached_body(todo, avail, bodies).await? else {
            // Claimed between the avail/ listing and now — settle on a later
            // run.
            continue;
        };
        let mut markers = 0usize;
        for client in &clients {
            if job_matches_client(&body, client) {
                todo.write_eligible(&client.client_id, &avail.job_id, avail.expires_at)
                    .await?;
                markers += 1;
            } else {
                todo.delete_eligible(&client.client_id, &avail.job_id, avail.expires_at)
                    .await?;
            }
        }
        todo.delete_pending_reindex_job(job_id).await?;
        tracing::info!(
            job_id = %job_id,
            markers_written = markers,
            "queue-maintenance: settled deferred reindex for recycled job"
        );
    }
    Ok(())
}

/// A marker is acted on only once *confirmed* orphaned: its storage `key` was
/// staged by the previous run (the second consecutive sighting) or — for a
/// marker a job owns — that job was positively removed by this run
/// (`terminal_jobs`, the same-run fast path). Returns whether to act now; a
/// first sighting is staged into `next` and returns `false`.
///
/// Client-owned-only markers (`suspended/`, `pending-reindex/`, and a lease
/// held by a deleted client) pass `owning_job = None`: no run positively
/// "removes" a client, so they always take the two-sighting path.
fn confirm_or_stage(
    prior: &HashSet<String>,
    terminal_jobs: &HashSet<JobId>,
    next: &mut HashSet<String>,
    key: String,
    owning_job: Option<&JobId>,
) -> bool {
    confirm_or_stage_group(prior, terminal_jobs, next, vec![key], owning_job)
}

/// The bulk counterpart to [`confirm_or_stage`], for a delete that clears many
/// markers in one call (a dead client's whole eligible partition, or a dead
/// job's every denial): confirm on the same rule applied to the whole set —
/// every key already staged last run, or (for a job-owned group) the job
/// positively removed this run. On a first sighting every key is staged.
///
/// `owning_job` carries the asymmetry between the two bulk cases: a dead job's
/// denials pass `Some(job)`, so a job terminated this run is swept in the same
/// run; a dead client's eligible markers pass `None`, because the client's
/// death — not any one job's termination — is what orphaned them, so they are
/// keyed on the client-death sighting in `prior` alone.
fn confirm_or_stage_group(
    prior: &HashSet<String>,
    terminal_jobs: &HashSet<JobId>,
    next: &mut HashSet<String>,
    keys: Vec<String>,
    owning_job: Option<&JobId>,
) -> bool {
    if owning_job.is_some_and(|job| terminal_jobs.contains(job))
        || keys.iter().all(|key| prior.contains(key))
    {
        true
    } else {
        next.extend(keys);
        false
    }
}

/// Reconcile every per-entity marker tree against the run's source of truth and
/// collect the detritus of entities that no longer exist. A job is live iff it
/// is in `live_jobs` (`avail/ ∪ leased/`); a client is live iff it is in
/// `live_clients` (the roster). A marker is an orphan when any owner is dead,
/// so one sweep covers both job liveness and client liveness — a job that
/// terminated, a client deleted while its job stays live, an orphaned
/// suspension, a failed `clients delete` — in one place.
///
/// Collection is gated by [`confirm_or_stage`], so nothing is removed until it
/// has been seen orphaned twice (or its job was positively removed this run).
/// The returned set is the next run's candidate set. Apart from
/// [`reindex_flagged_clients`], which drops a deleted client's eligible
/// partition immediately when a surviving reindex flag makes it visit that
/// client, this is the only place markers for terminal entities are removed.
///
/// Deletion granularity mirrors the store surface: a fully-dead client's
/// eligible partition and a dead job's denials each collapse to one bulk
/// delete, while the cross-cause cases (a live job's marker for a dead client,
/// or vice versa) delete the single marker so a living owner's other markers
/// survive. A lease held by a deleted client is **recycled**, not deleted:
/// deleting would destroy the only copy of the job body, so it is returned to
/// `avail/` (via [`resolve_or_recycle_lease`], which deletes it instead only
/// when the job already has a submission record).
///
/// **Failure policy — fail fast, don't soldier on per item.** Every store call
/// propagates with `?`, so the first error aborts the whole sweep; passes 1–3
/// instead log-and-skip per item. The asymmetry is deliberate: those passes
/// restore claimability, where one stuck item strands a job, whereas this sweep
/// only reclaims inert detritus — a deferred pass costs nothing but a little
/// disk. The realistic failure here is also correlated (an S3 outage or
/// throttling), where the next call is as likely to fail as this one and the
/// SDK has already exhausted its own retry/backoff before surfacing the error;
/// soldiering on would just replay that storm against an endpoint already
/// shedding load. Aborting sheds load too, and `run()` degrades it safely: the
/// candidate set is cleared at the start of the run and rewritten only on
/// success, so a failed run replays the two-sighting clock one interval later.
async fn reconcile_orphans(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    live_jobs: &HashSet<JobId>,
    live_clients: &HashSet<ClientId>,
    terminal_jobs: &HashSet<JobId>,
    prior: &HashSet<String>,
) -> anyhow::Result<HashSet<String>> {
    let mut next = HashSet::new();

    // ── eligible/clients/{client}/{job}_{exp} ── orphan iff job or client dead.
    // A dead client's whole partition is dropped in one call, so its orphans
    // are grouped and confirmed together (a client absent from the roster gains
    // no new markers between runs, so its keys move in lockstep); a live
    // client's marker for a dead job is deleted individually.
    let mut dead_client_eligible: BTreeMap<ClientId, Vec<(String, JobId, ExpiresAt)>> =
        BTreeMap::new();
    let mut eligible_removed = 0usize;
    for (client_id, job_id, expires_at) in todo.list_all_eligible().await? {
        let client_dead = !live_clients.contains(&client_id);
        let job_dead = !live_jobs.contains(&job_id);
        if !client_dead && !job_dead {
            continue;
        }
        let key = format!(
            "eligible/clients/{}/{}",
            client_id,
            eligible_filename(&job_id, expires_at)
        );
        if client_dead {
            dead_client_eligible
                .entry(client_id)
                .or_default()
                .push((key, job_id, expires_at));
        } else if confirm_or_stage(prior, terminal_jobs, &mut next, key, Some(&job_id)) {
            todo.delete_eligible(&client_id, &job_id, expires_at)
                .await?;
            eligible_removed += 1;
        }
    }
    for (client_id, markers) in dead_client_eligible {
        let count = markers.len();
        let keys = markers.into_iter().map(|(key, _, _)| key).collect();
        if confirm_or_stage_group(prior, terminal_jobs, &mut next, keys, None) {
            todo.delete_eligible_for_client(&client_id).await?;
            eligible_removed += count;
            tracing::info!(
                client_id = %client_id,
                "queue-maintenance: reconciled eligible markers for a client no longer in the roster"
            );
        }
    }

    // ── denied/{job}.{client} ── orphan iff job or client dead. Symmetric to
    // eligible: a dead job's denials collapse to one `delete_denied_for_job`;
    // a live job's denial by a dead client deletes the single marker so the
    // job's other (live-client) denials — which still gate escalation — stay.
    let mut dead_job_denied: BTreeMap<JobId, Vec<(String, ClientId)>> = BTreeMap::new();
    let mut denied_removed = 0usize;
    for (job_id, client_id) in todo.list_all_denied().await? {
        let job_dead = !live_jobs.contains(&job_id);
        let client_dead = !live_clients.contains(&client_id);
        if !job_dead && !client_dead {
            continue;
        }
        let key = format!("denied/{}.{}", job_id, client_id);
        if job_dead {
            dead_job_denied
                .entry(job_id)
                .or_default()
                .push((key, client_id));
        } else if confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            todo.delete_denied(&job_id, &client_id).await?;
            denied_removed += 1;
        }
    }
    for (job_id, markers) in dead_job_denied {
        let count = markers.len();
        let keys = markers.into_iter().map(|(key, _)| key).collect();
        if confirm_or_stage_group(prior, terminal_jobs, &mut next, keys, Some(&job_id)) {
            todo.delete_denied_for_job(&job_id).await?;
            denied_removed += count;
        }
    }

    // ── suspended/{client}.json ── orphan iff client dead. Owned by the client
    // alone (its `conflicting_job_id` records why, not who owns it), so job
    // liveness is irrelevant — a live client's suspension is cleared only by
    // `unsuspend`.
    let mut suspensions_removed = 0usize;
    for (client_id, _record) in todo.list_suspensions().await? {
        if live_clients.contains(&client_id) {
            continue;
        }
        let key = format!("suspended/{}.json", client_id);
        if confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            todo.delete_suspension(&client_id).await?;
            suspensions_removed += 1;
            tracing::info!(
                client_id = %client_id,
                "queue-maintenance: deleted orphaned suspension for deleted client"
            );
        }
    }

    // ── pending-reindex/{client}.{uuid} ── orphan iff client dead. The reindex
    // pass already drops these when it visits a flagged-but-deleted client;
    // this is the backstop for a flag its best-effort delete left behind.
    let mut reindex_flags_removed = 0usize;
    for (client_id, raw_key) in todo.list_pending_reindex().await? {
        if live_clients.contains(&client_id) {
            continue;
        }
        let key = format!("pending-reindex/{}", raw_key);
        if confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            todo.delete_pending_reindex(&raw_key).await?;
            reindex_flags_removed += 1;
        }
    }

    // ── pending-reindex-jobs/{job} ── orphan iff job dead. The settle pass
    // clears these as jobs return to `avail/` or gain a record, but a job
    // planner-deleted while flagged (terminal, never recorded) keeps its flag
    // there forever — this collects it. A *leased* job is live (in `live_jobs`)
    // and keeps its flag for the settle pass to consume once it recycles.
    let mut reindex_job_flags_removed = 0usize;
    for job_id in todo.list_pending_reindex_jobs().await? {
        if live_jobs.contains(&job_id) {
            continue;
        }
        let key = format!("pending-reindex-jobs/{}", job_id);
        if confirm_or_stage(prior, terminal_jobs, &mut next, key, Some(&job_id)) {
            todo.delete_pending_reindex_job(&job_id).await?;
            reindex_job_flags_removed += 1;
        }
    }

    // ── leased/{client}/{job}_{exp} ── a lease held by a deleted client is
    // recycled (not deleted): the job returns to `avail/` and becomes
    // claimable again, its body intact. A live client's lease is untouched here
    // — the expired-lease pass owns that. An unparseable key is foreign cruft
    // already warned by that pass.
    let mut leases_recycled = 0usize;
    for raw_key in todo.list_leased().await? {
        let Ok((job_id, client_id, lease_expiry)) = parse_leased_key(&raw_key) else {
            continue;
        };
        if live_clients.contains(&client_id) {
            continue;
        }
        let key = format!("leased/{}", raw_key);
        if !confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            continue;
        }
        match resolve_or_recycle_lease(todo, submissions, &job_id, &client_id, lease_expiry).await?
        {
            LeaseResolution::Recycled => {
                tracing::info!(
                    job_id = %job_id,
                    client_id = %client_id,
                    "queue-maintenance: recycled lease held by deleted client to avail/"
                );
                leases_recycled += 1;
            }
            LeaseResolution::ResolvedStale => {
                tracing::info!(
                    job_id = %job_id,
                    client_id = %client_id,
                    "queue-maintenance: deleted-client lease already had a submission record; deleted stale lease"
                );
            }
            LeaseResolution::Gone => {
                tracing::debug!(job_id = %job_id, client_id = %client_id, "deleted-client lease already resolved elsewhere");
            }
        }
    }

    if eligible_removed
        + denied_removed
        + suspensions_removed
        + reindex_flags_removed
        + reindex_job_flags_removed
        + leases_recycled
        > 0
    {
        tracing::info!(
            eligible_removed,
            denied_removed,
            suspensions_removed,
            reindex_flags_removed,
            reindex_job_flags_removed,
            leases_recycled,
            "queue-maintenance: reconciled orphaned todo/ markers"
        );
    }
    Ok(next)
}

/// Delete `tmp/` partial job files older than `max_age` — debris from a
/// planner that crashed between writing to `tmp/` and the atomic promote to
/// `avail/`. On S3 an equivalent lifecycle rule on the `todo/tmp/` prefix may
/// replace this pass (`docs/operations.md` §3.1).
async fn cleanup_stale_tmp(todo: &dyn TodoStore, max_age: Duration) -> anyhow::Result<()> {
    let stale = todo.list_stale_tmp(max_age).await?;
    if stale.is_empty() {
        return Ok(());
    }
    // Sequential async deletes stay a plain loop: nothing to accumulate and no
    // concurrency to gain, so a `stream` would only add `.map(Ok)` ceremony.
    for key in &stale {
        todo.delete_tmp_object(key).await?;
    }
    tracing::info!(
        files_deleted = stale.len(),
        "queue-maintenance: deleted stale tmp/ files"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;
    use crate::stores::{
        LocalFsSubmissionStore, build_local_fs_auth_store, build_local_fs_todo_store,
    };
    use crate::todo_filename::leased_key;
    use crate::types::ClientId;
    use rstest::rstest;

    /// The recycle pass reports every lease it listed — live and expired
    /// alike — so [`run`] can count those jobs as live for the GC sweeps even
    /// when a concurrent recycle moves one out of `leased/` before the later
    /// listings (see the live-set comment in [`run`]). An expired lease whose
    /// job already has a submission record is a teardown leftover: deleted
    /// rather than recycled, and reported as resolved.
    #[tokio::test]
    async fn recycle_pass_reports_live_and_expired_leases_as_seen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = build_local_fs_todo_store(&StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        })?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let leased_dir = dir.path().join("todo").join("leased");

        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let client = ClientId::try_new("client1")?;
        [
            ("job-expired", "2026-01-01T00:00:00Z"),
            ("job-live", "2026-12-31T00:00:00Z"),
            ("job-recorded", "2026-01-01T00:00:00Z"),
        ]
        .into_iter()
        .try_for_each(|(job, expiry)| -> anyhow::Result<()> {
            let path = leased_dir.join(leased_key(
                &JobId::new_unchecked(job),
                &client,
                expiry.parse()?,
            ));
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("leased key has no parent dir"))?;
            std::fs::create_dir_all(parent)?;
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({ "job_id": job }))?,
            )?;
            Ok(())
        })?;
        let recorded = JobId::new_unchecked("job-recorded");
        submissions
            .write_processed(
                &recorded,
                &serde_json::json!({ "job_id": "job-recorded", "message_type": "success" }),
            )
            .await?;

        let (seen, resolved, failures) = recycle_expired_leases(&*todo, &submissions, now).await?;

        assert_eq!(failures, 0);
        assert!(seen.contains(&JobId::new_unchecked("job-expired")));
        assert!(seen.contains(&JobId::new_unchecked("job-live")));
        assert!(seen.contains(&recorded));
        assert_eq!(resolved, HashSet::from([recorded.clone()]));
        // The expired lease was recycled to avail/; the recorded job's stale
        // lease was deleted, not recycled; the live lease still holds.
        assert!(
            todo.get_avail_by_job(&JobId::new_unchecked("job-expired"))
                .await?
                .is_some()
        );
        assert!(todo.get_avail_by_job(&recorded).await?.is_none());
        assert_eq!(todo.list_leased().await?.len(), 1);
        Ok(())
    }

    fn settle_test_client(id: &str, pk_suffix: &str, chip: &str) -> anyhow::Result<Client> {
        // 64-char hex pubkey with a distinct 2-char suffix so clients differ.
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: crate::validated::PublicKeyHex::try_new(format!(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567{pk_suffix}"
            ))?,
            organization: crate::validated::NonEmptyTrimmedString::try_new("org")?,
            client_details: crate::validated::NonEmptyTrimmedString::try_new("details")?,
            contact_email: crate::validated::ContactEmail::try_new("a@b.com")?,
            status: crate::client::ClientStatus::Approved,
            registered_at: Utc::now(),
            device_profile: crate::client::DeviceProfile {
                device_chip_model: Some(crate::validated::NonEmptyTrimmedString::try_new(chip)?),
                ..Default::default()
            },
            capabilities: Default::default(),
        })
    }

    /// Settling matches each flagged job against the client records **current
    /// at settle time** — read from the auth store inside the pass, never a
    /// snapshot taken earlier in the run. By the time this pass runs,
    /// `reindex_flagged_clients` may already have consumed a client's reindex
    /// flag for a newer record, so a marker written here from an older one
    /// would never be repaired (see the fresh-read note on
    /// [`settle_pending_reindex_jobs`]). Pinned by seeding a marker that
    /// contradicts the stored records and checking the pass supersedes it in
    /// both directions: the stale marker is deleted, the missing one written.
    #[tokio::test]
    async fn settle_matches_against_current_client_records() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        };
        let todo = build_local_fs_todo_store(&config)?;
        let auth = build_local_fs_auth_store(&config)?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));

        // Stored records: `mismatched` no longer satisfies the job's rule but
        // holds a marker from its superseded profile; `matched` satisfies it
        // and holds none.
        let mismatched = settle_test_client("settle-mismatch", "00", "chip-b")?;
        let matched = settle_test_client("settle-match", "11", "chip-a")?;
        auth.put_client(&mismatched).await?;
        auth.put_client(&matched).await?;

        let job = JobId::new_unchecked("job-settle");
        todo.write_eligible(&mismatched.client_id, &job, ExpiresAt::Never)
            .await?;
        todo.write_pending_reindex_job(&job).await?;

        let avail = AvailKey {
            key: "avail/job-settle".into(),
            job_id: job.clone(),
            expires_at: ExpiresAt::Never,
        };
        let mut bodies = HashMap::from([(
            avail.key.clone(),
            serde_json::json!({
                "job_id": "job-settle",
                "requires": ["chip:chip-a"],
            }),
        )]);

        settle_pending_reindex_jobs(&*todo, &*auth, &submissions, &[avail], &mut bodies).await?;

        assert_eq!(
            todo.list_all_eligible().await?,
            vec![(matched.client_id.clone(), job.clone(), ExpiresAt::Never)]
        );
        assert!(todo.list_pending_reindex_jobs().await?.is_empty());
        Ok(())
    }

    // ── orphan reconciliation ──────────────────────────────────────────────

    /// A `TodoStore` + `SubmissionStore` pair over one tempdir, for the
    /// reconciliation-sweep tests. The `TempDir` is returned so the caller
    /// keeps it alive for the test's duration.
    fn recon_stores() -> anyhow::Result<(
        std::sync::Arc<dyn TodoStore>,
        LocalFsSubmissionStore,
        tempfile::TempDir,
    )> {
        let dir = tempfile::tempdir()?;
        let todo = build_local_fs_todo_store(&StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        })?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        Ok((todo, submissions, dir))
    }

    /// Seed one `leased/{client}/{job}_{expiry}.json` entry directly on disk
    /// (its body carries no `expires_at`, so a recycle treats it as `never`).
    fn seed_lease(
        data_dir: &std::path::Path,
        job: &JobId,
        client: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let path = data_dir
            .join("todo")
            .join("leased")
            .join(leased_key(job, client, expiry));
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("leased key has no parent dir"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "job_id": job.to_string() }))?,
        )?;
        Ok(())
    }

    /// A client's markers across every subtree — eligible, denied, suspended,
    /// pending-reindex, and a held lease, all referencing *live* jobs so only
    /// the client's liveness is in play — are collected within two sweeps once
    /// the client leaves the roster (its lease recycled to `avail/`, body
    /// intact) and left untouched for as long as it stays live. Neither case
    /// removes anything on the first sighting.
    #[rstest]
    #[case::deleted_client_collected(false)]
    #[case::live_client_untouched(true)]
    #[tokio::test]
    async fn reconcile_client_owned_markers_across_subtrees(
        #[case] client_live: bool,
    ) -> anyhow::Result<()> {
        let (todo, submissions, dir) = recon_stores()?;
        let client = ClientId::try_new(if client_live { "live-client" } else { "ghost" })?;
        let job = JobId::new_unchecked("job-live");
        let job_leased = JobId::new_unchecked("job-leased");

        todo.write_eligible(&client, &job, ExpiresAt::Never).await?;
        todo.write_denied(&job, &client).await?;
        todo.write_suspension(&client, "2026-06-01T00:00:00Z".parse()?, &job)
            .await?;
        todo.write_pending_reindex(&client).await?;
        seed_lease(
            dir.path(),
            &job_leased,
            &client,
            "2026-12-31T00:00:00Z".parse()?,
        )?;

        let live_jobs = HashSet::from([job.clone(), job_leased.clone()]);
        let live_clients = if client_live {
            HashSet::from([client.clone()])
        } else {
            HashSet::new()
        };
        let terminal: HashSet<JobId> = HashSet::new();

        // First sweep never removes anything; a dead client's keys are only
        // staged, a live client's are skipped entirely.
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_all_eligible().await?.len(), 1);
        assert_eq!(todo.list_all_denied().await?.len(), 1);
        assert_eq!(todo.list_suspensions().await?.len(), 1);
        assert_eq!(todo.list_pending_reindex().await?.len(), 1);
        assert_eq!(todo.list_leased().await?.len(), 1);
        assert_eq!(candidates.len(), if client_live { 0 } else { 5 });

        // Second sweep confirms and collects everything for a dead client (lease
        // recycled), or leaves a live client's markers in place.
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &candidates,
        )
        .await?;
        let remaining = usize::from(client_live);
        assert_eq!(todo.list_all_eligible().await?.len(), remaining);
        assert_eq!(todo.list_all_denied().await?.len(), remaining);
        assert_eq!(todo.list_suspensions().await?.len(), remaining);
        assert_eq!(todo.list_pending_reindex().await?.len(), remaining);
        assert_eq!(todo.list_leased().await?.len(), remaining);
        // The lease is recycled to avail/ (body preserved) only for a dead client.
        assert_eq!(
            todo.get_avail_by_job(&job_leased).await?.is_some(),
            !client_live
        );
        assert!(candidates.is_empty());
        Ok(())
    }

    /// The contrast to recycling: a lease held by a deleted client whose job
    /// *already has a submission record* is deleted — finishing the teardown —
    /// not recycled, since recycling would make a finished job claimable again.
    /// The job never returns to `avail/`.
    #[tokio::test]
    async fn reconcile_deletes_deleted_client_lease_when_job_recorded() -> anyhow::Result<()> {
        let (todo, submissions, dir) = recon_stores()?;
        let ghost = ClientId::try_new("ghost")?;
        let job = JobId::new_unchecked("job-recorded");

        seed_lease(dir.path(), &job, &ghost, "2026-12-31T00:00:00Z".parse()?)?;
        submissions
            .write_processed(
                &job,
                &serde_json::json!({ "job_id": "job-recorded", "message_type": "success" }),
            )
            .await?;

        let live_jobs: HashSet<JobId> = HashSet::new();
        let live_clients: HashSet<ClientId> = HashSet::new();
        let terminal: HashSet<JobId> = HashSet::new();

        // First sweep stages the lease but removes nothing.
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_leased().await?.len(), 1);
        assert_eq!(candidates.len(), 1);

        // Second sweep deletes the stale lease; the recorded job is not recycled.
        reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &candidates,
        )
        .await?;
        assert!(todo.list_leased().await?.is_empty());
        assert!(todo.get_avail_by_job(&job).await?.is_none());
        Ok(())
    }

    /// Both orphaning directions converge — a job-dead marker under a live
    /// client and a client-dead marker under a live job — and the per-marker
    /// denied delete spares the same live job's denials from clients that still
    /// exist (it must not collapse to the whole job).
    #[tokio::test]
    async fn reconcile_collects_both_orphan_directions() -> anyhow::Result<()> {
        let (todo, submissions, _dir) = recon_stores()?;
        let live_client = ClientId::try_new("live-client")?;
        let dead_client = ClientId::try_new("dead-client")?;
        let live_job = JobId::new_unchecked("job-live");
        let dead_job = JobId::new_unchecked("job-dead");

        todo.write_eligible(&live_client, &dead_job, ExpiresAt::Never)
            .await?;
        todo.write_eligible(&dead_client, &live_job, ExpiresAt::Never)
            .await?;
        todo.write_denied(&live_job, &dead_client).await?;
        todo.write_denied(&live_job, &live_client).await?;

        let live_jobs = HashSet::from([live_job.clone()]);
        let live_clients = HashSet::from([live_client.clone()]);
        let terminal: HashSet<JobId> = HashSet::new();

        // Two sweeps to clear the two-sighting gate.
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &HashSet::new(),
        )
        .await?;
        reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &candidates,
        )
        .await?;

        assert!(todo.list_all_eligible().await?.is_empty());
        assert_eq!(
            todo.list_all_denied().await?,
            vec![(live_job.clone(), live_client.clone())]
        );
        Ok(())
    }

    /// A job positively removed this run (`terminal_jobs`) has its markers swept
    /// in the same run — no second sighting needed.
    #[tokio::test]
    async fn reconcile_sweeps_terminal_job_markers_same_run() -> anyhow::Result<()> {
        let (todo, submissions, _dir) = recon_stores()?;
        let client = ClientId::try_new("live-client")?;
        let job = JobId::new_unchecked("job-terminal");

        todo.write_eligible(&client, &job, ExpiresAt::Never).await?;
        todo.write_denied(&job, &client).await?;

        let live_jobs: HashSet<JobId> = HashSet::new();
        let live_clients = HashSet::from([client.clone()]);
        let terminal = HashSet::from([job.clone()]);

        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert!(todo.list_all_eligible().await?.is_empty());
        assert!(todo.list_all_denied().await?.is_empty());
        assert!(candidates.is_empty());
        Ok(())
    }

    /// A candidate whose owner reappears between runs is spared: on the second
    /// sighting the client is live again, so its marker is neither deleted nor
    /// re-staged (the mechanism that makes multiple passes safe).
    #[tokio::test]
    async fn reconcile_spares_reappearing_client() -> anyhow::Result<()> {
        let (todo, submissions, _dir) = recon_stores()?;
        let client = ClientId::try_new("flapper")?;
        let job = JobId::new_unchecked("job-x");
        todo.write_eligible(&client, &job, ExpiresAt::Never).await?;

        let live_jobs = HashSet::from([job.clone()]);
        let terminal: HashSet<JobId> = HashSet::new();

        // First sweep: client absent → the marker is staged.
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &HashSet::new(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_all_eligible().await?.len(), 1);
        assert_eq!(candidates.len(), 1);

        // Second sweep: the client reappeared → spared, candidate cleared.
        let live_clients = HashSet::from([client.clone()]);
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &candidates,
        )
        .await?;
        assert_eq!(todo.list_all_eligible().await?.len(), 1);
        assert!(candidates.is_empty());
        Ok(())
    }

    /// A `pending-reindex-jobs/` flag for a dead job (planner-deleted, never
    /// recorded) is collected on the second sweep, while a flag for a live job
    /// is spared — closing the settle pass's forever-linger without disturbing
    /// a job still awaiting settlement.
    #[tokio::test]
    async fn reconcile_collects_pending_reindex_job_flag_for_dead_job() -> anyhow::Result<()> {
        let (todo, submissions, _dir) = recon_stores()?;
        let dead_job = JobId::new_unchecked("job-dead");
        let live_job = JobId::new_unchecked("job-live");
        todo.write_pending_reindex_job(&dead_job).await?;
        todo.write_pending_reindex_job(&live_job).await?;

        let live_jobs = HashSet::from([live_job.clone()]);
        let live_clients: HashSet<ClientId> = HashSet::new();
        let terminal: HashSet<JobId> = HashSet::new();

        // First sweep stages the dead job's flag but removes nothing.
        let candidates = reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_pending_reindex_jobs().await?.len(), 2);

        // Second sweep collects the dead job's flag; the live job's is spared.
        reconcile_orphans(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &terminal,
            &candidates,
        )
        .await?;
        assert_eq!(
            todo.list_pending_reindex_jobs().await?,
            vec![live_job.clone()]
        );
        Ok(())
    }
}
