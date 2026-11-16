//! The `pipette-mgmt queue-maintenance` command: all cron reconciliation for
//! the job queue. Runs must be serialized and the host clock must not
//! run ahead of `serve`'s (`docs/operations.md`); see `docs/queue.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream;
use futures::stream::StreamExt;
use serde_json::Value;

use pipette_core::benchmark::Benchmark;
use pipette_core::client::Client;
use pipette_core::denial::{DeniedClient, prevailing_failure_code, summarize_denials};
use pipette_core::matching;
use pipette_core::matching::{FleetTags, capability_clauses_malformed, job_matches_client};
use pipette_core::plan::PlanManifest;
use pipette_core::todo_filename::Lease;
use pipette_plan_types::{
    BenchmarkId, ClientId, ExpiresAt, FailureCode, FailureSource, JobId, PlanId, PlanStatus,
};
use pipette_store::decode_stats::{DecodeCounts, S3_DECODE};
use pipette_store::http_stats::{HttpCounts, S3_HTTP};
use pipette_store::{
    AvailEntry, ClientStore, CreateOutcome, JobPlanMarker, PlanStore, RecycleResult, RedateResult,
    ReindexJob, ReindexRequest, STORAGE_CONCURRENCY, SubmissionStore, TagStore, TodoStore,
    drain_clients, drain_plans, warn_unreadable_clients,
};

use crate::submission::{UnrecordableJobError, record_system_failure};
use crate::{plan_ingestion, plan_maintenance};

/// The settings [`run`] takes from config. Named fields, because two bare
/// `Duration` arguments are swappable at a call site without a compile error.
pub struct MaintenanceSettings {
    /// `todo_tmp_max_age_secs`.
    pub tmp_max: Duration,
    /// `plan_creating_max_age_secs`.
    pub plan_creating_max: Duration,
    /// `plan_lost_lease_limit`.
    pub lost_lease_limit: usize,
}

/// Run every queue-maintenance pass. Safe to run repeatedly; eventually
/// consistent within one cron interval.
pub async fn run(
    todo: &dyn TodoStore,
    auth: &dyn ClientStore,
    tags: &dyn TagStore,
    submissions: &dyn SubmissionStore,
    plans: &dyn PlanStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    settings: MaintenanceSettings,
) -> anyhow::Result<()> {
    let bodies_before = todo.queue_body_reads();
    let at_start = PhaseMark::now();
    let now = Utc::now();
    // Cleared at once, so a failed run leaves no sightings a later run would
    // mistake as consecutive; GC is then delayed one interval, the safe side.
    let prior_gc_candidates = plans.read_gc_candidates().await?;
    plans.write_gc_candidates(&HashSet::new()).await?;

    // Markers before manifests: the reverse could show a fresh plan's marker
    // with no manifest, which reads as an orphan. Manifests before the queue:
    // ingestion promotes a plan's jobs before its manifest leaves `creating`,
    // so a plan read past `creating` cannot miss its jobs and latch `complete`.
    // Ahead of the recycle too, so the recycle's `leased/` listing stays next to `avail/`'s.
    let cancel_markers: HashSet<PlanId> = plans.list_cancel_markers().await?.into_iter().collect();
    let plan_listing = drain_plans(plans, None).await?;
    // No pass reconciles an unreadable manifest, so it is logged by name on
    // every run until an operator removes it.
    plan_listing.unreadable.iter().for_each(|bad| {
        tracing::warn!(manifest = %bad.key, reason = %bad.reason, "queue-maintenance: unreadable plan manifest, skipped");
    });
    let mut manifests = plan_listing.items;

    let (leased_at_recycle, resolved_at_recycle, failed_items) =
        settle_expired_leases(todo, submissions, catalog, settings.lost_lease_limit, now).await?;

    // Listed after the recycle so recycled jobs are included. A job racing
    // these listings can miss the live set once; two-sighting GC spares it.
    let avail_keys = todo.list_avail_entries().await?;
    let leases = todo.list_leases().await?;

    // Teardown precedes every queue reader, so no pass indexes a doomed job.
    // One `job-plans/` listing per run, so a plan with many jobs costs none.
    let job_plan_markers = plans.list_all_job_plan_markers().await?;
    let membership = plan_maintenance::Membership::new(job_plan_markers.clone());
    let teardown = plan_maintenance::tear_down_plans(
        plans,
        &manifests,
        &cancel_markers,
        &membership,
        avail_keys,
        leases,
        now,
        settings.plan_creating_max,
    )
    .await?;
    let failed_items = failed_items + teardown.failures;
    let leased_ids: HashSet<JobId> = teardown.leased.iter().map(|l| l.job_id.clone()).collect();

    // Before the expiry pass, which re-dates or retires a job from the member
    // horizons left after the lapses.
    let failed_items = failed_items
        + plan_maintenance::record_member_lapses(
            plans,
            submissions,
            &mut manifests,
            &job_plan_markers,
            &cancel_markers,
            &teardown.endings.stranded,
            now,
            settings.plan_creating_max,
        )
        .await;

    let (all_keys, expired_now, expiry_failures) =
        expire_overdue_jobs(todo, submissions, plans, catalog, teardown.avail, now).await?;

    // A promoted job waits in avail/ until at least one of its plans leaves
    // creating. Keep it out of the flagged read and the passes after it, so it
    // keeps its reindex flag and the run reads its body only after its plan
    // becomes active.
    let index_candidates: HashSet<JobId> = all_keys
        .iter()
        .map(|key| key.job_id.clone())
        .chain(teardown.leased.iter().map(|lease| lease.job_id.clone()))
        .collect();
    let withheld = uncommitted_jobs(&index_candidates, &job_plan_markers, &manifests);
    let listed: Vec<JobId> = all_keys
        .iter()
        .map(|key| key.job_id.clone())
        .filter(|job_id| !withheld.contains(job_id))
        .collect();
    let batch = todo.jobs_needing_reindex(&listed).await?;
    let unreadable: HashSet<JobId> = batch.unreadable.into_iter().collect();
    let mut flagged = batch.jobs;
    let failed_items = failed_items + unreadable.len();
    let flagged_ids: HashSet<JobId> = flagged.iter().map(|job| job.job_id.clone()).collect();
    // The cache owns each flagged body; the index pass borrows it back below.
    let mut bodies: HashMap<AvailEntry, Value> = flagged
        .iter_mut()
        .map(|job| (reindex_entry(job), std::mem::take(&mut job.body)))
        .collect();
    // Retirement first, so an unmatchable body is not reported as an exhausted
    // roster.
    let (all_keys, retired_now, retirement_failures) = retire_unmatchable_jobs(
        todo,
        submissions,
        catalog,
        all_keys,
        &flagged_ids,
        &mut bodies,
        now,
    )
    .await?;
    let (all_keys, escalated_now, escalation_failures) = escalate_all_denied_jobs(
        todo,
        submissions,
        catalog,
        all_keys,
        &flagged_ids,
        &mut bodies,
        now,
    )
    .await?;
    let failed_items =
        failed_items + expiry_failures + retirement_failures.len() + escalation_failures.len();

    let indexable_ids: Vec<JobId> = all_keys
        .iter()
        .map(|key| key.job_id.clone())
        .filter(|id| !withheld.contains(id) && !unreadable.contains(id))
        .collect();

    // Removed by this run, so the live set leaves them out.
    let mut terminal_now = expired_now;
    terminal_now.extend(retired_now);
    terminal_now.extend(escalated_now);
    terminal_now.extend(resolved_at_recycle);
    terminal_now.extend(teardown.retired);

    let clients = drain_clients(auth).await?;
    warn_unreadable_clients(&clients.unreadable);
    let client_records_drained = clients.items.len() + clients.unreadable.len();
    let clients = clients.items;
    let fleet_tags = FleetTags::new(tags.list_forward_tag_markers().await?);
    // A leased job is live: a marker swept mid-lease is never rebuilt. Only
    // `leased_at_recycle` drops `terminal_now`; the fresh views are live as is.
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
    // One fleet snapshot for every index pass and plan reconciliation.
    // Unfinished joiners stay out of the persisted indexed-worker set.
    let rights = matching::FleetRights::new(auth.list_client_permissions().await?);
    let workers = rights.workers(&clients);
    let worker_ids: HashSet<ClientId> = workers.iter().map(|c| c.client_id.clone()).collect();
    tracing::info!(
        avail_jobs = all_keys.len(),
        live_jobs = live_ids.len(),
        clients = clients.len(),
        workers = workers.len(),
        // Every plan is kept, so this is what grows the run.
        plans = manifests.len(),
        "queue-maintenance: eligible-index pass starting"
    );

    let indexed_workers = todo.read_workers().await?;
    let joined = track_worker_changes(todo, &indexed_workers, &worker_ids).await?;
    // Before the client rebuilds: they read each record fresh, so a matching
    // input that changed after the snapshot wins.
    let indexable: HashSet<JobId> = indexable_ids
        .iter()
        .filter(|id| !retirement_failures.contains(*id) && !escalation_failures.contains(*id))
        .cloned()
        .collect();
    // A job without a cached body keeps its flag for the next run.
    let to_index: Vec<ReindexJob> = flagged
        .into_iter()
        .filter(|job| indexable.contains(&job.job_id))
        .filter_map(|mut job| {
            job.body = bodies.remove(&reindex_entry(&job))?;
            Some(job)
        })
        .collect();
    let indexed = index_flagged_jobs(todo, &workers, &fleet_tags, &to_index).await?;
    bodies.extend(
        to_index
            .into_iter()
            .map(|job| (reindex_entry(&job), job.body)),
    );
    let fallback: HashSet<JobId> = flagged_ids.difference(&indexed).cloned().collect();
    let retry_joiners = reindex_flagged_clients(
        todo,
        auth,
        tags,
        &worker_ids,
        &joined,
        &indexable_ids,
        &leased_ids,
    )
    .await?;
    let indexed_next: HashSet<ClientId> = worker_ids.difference(&retry_joiners).cloned().collect();
    // Only a successful rebuild consumes a joiner's retry trigger.
    if indexed_workers != indexed_next {
        todo.write_workers(&indexed_next).await?;
    }

    // A marker is claimable only at its job's current deadline. A job with
    // two `avail/` entries is omitted.
    let avail_expiries: HashMap<JobId, ExpiresAt> = all_keys
        .iter()
        .fold(
            HashMap::new(),
            |mut acc: HashMap<JobId, Option<ExpiresAt>>, a| {
                acc.entry(a.job_id.clone())
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(a.expires_at));
                acc
            },
        )
        .into_iter()
        .filter_map(|(job_id, expiry)| expiry.map(|expiry| (job_id, expiry)))
        .collect();
    let mut eligible = todo.list_all_eligible().await?;
    for (client_id, job_id, expires_at) in eligible
        .iter()
        .filter(|(_, job_id, _)| withheld.contains(job_id))
    {
        todo.delete_eligible(client_id, job_id, *expires_at).await?;
    }
    eligible.retain(|(_, job_id, _)| !withheld.contains(job_id));
    let denied = todo.list_all_denied().await?;
    let claimable = claimable_markers(&eligible, &avail_expiries, &denied, now);
    if let Err(e) = submissions.repair_outcomes().await {
        tracing::warn!(error = %e, "queue-maintenance: submission outcome repair failed");
    }

    // Needs the post-teardown views and the client roster built above.
    let after_queue_passes = PhaseMark::now();
    let reconciled = plan_maintenance::reconcile_plans(
        plans,
        todo,
        submissions,
        &manifests,
        &cancel_markers,
        &teardown.endings,
        &membership,
        &all_keys,
        &leased_ids,
        &live_ids,
        &workers,
        &fleet_tags,
        &claimable.iter().map(|(_, job, _)| job.clone()).collect(),
        &fallback,
        &denied,
        &mut bodies,
        now,
        settings.plan_creating_max,
    )
    .await?;

    let after_reconcile = PhaseMark::now();
    let index_gc = plan_maintenance::gc_plan_index(
        plans,
        &manifests,
        &job_plan_markers,
        &live_ids,
        &prior_gc_candidates,
        now,
        settings.plan_creating_max,
    )
    .await;
    // One persisted set for every keyspace, written after the last collecting
    // pass so a run that fails earlier stages nothing.
    let (source_candidates, source_gc_failures) = plan_maintenance::gc_pending_plan_sources(
        plans,
        &prior_gc_candidates,
        now,
        settings.plan_creating_max,
    )
    .await;
    let after_gc = PhaseMark::now();
    let next_candidates: HashSet<String> = index_gc
        .candidates
        .into_iter()
        .chain(source_candidates)
        .collect();
    plans.write_gc_candidates(&next_candidates).await?;

    let failed_items = failed_items + reconciled.failures + index_gc.failures + source_gc_failures;

    cleanup_stale_tmp(todo, settings.tmp_max).await?;
    // Last: the passes above act on the deadlines they listed. A fresh clock, so
    // no row moves onto a horizon that passed during the run.
    let deadlines_restamped = todo.restamp_avail_deadlines(Utc::now()).await?;

    [
        ("queue_passes", &at_start, &after_queue_passes),
        ("reconcile_plans", &after_queue_passes, &after_reconcile),
        ("plan_index_gc", &after_reconcile, &after_gc),
    ]
    .into_iter()
    .for_each(|(phase, earlier, later)| log_phase_work(phase, earlier, later));
    let run_cpu = process_cpu_time()
        .zip(at_start.cpu)
        .map(|(now, start)| now.since(start));
    // The candidate-set write, `tmp/` cleanup and the re-stamp are in no phase,
    // so the phases sum to less than `duration_ms`.
    tracing::info!(
        duration_ms = millis(at_start.at.elapsed()),
        queue_passes_ms = millis(after_queue_passes.at - at_start.at),
        reconcile_plans_ms = millis(after_reconcile.at - after_queue_passes.at),
        plan_index_gc_ms = millis(after_gc.at - after_reconcile.at),
        plans = manifests.len(),
        plans_reconciled = reconciled.reconciled,
        count_jobs_probes = reconciled.probes,
        count_jobs_ms = millis(reconciled.count_jobs),
        starvation_ms = millis(reconciled.starvation),
        put_plan_ms = millis(reconciled.put_plan),
        cancel_markers_ms = millis(reconciled.cancel_markers),
        slowest_plan_id = reconciled
            .slowest
            .as_ref()
            .map(|slowest| tracing::field::display(&slowest.plan_id)),
        slowest_plan_ms = reconciled
            .slowest
            .as_ref()
            .map(|slowest| millis(slowest.elapsed)),
        slowest_plan_probes = reconciled.slowest.as_ref().map(|slowest| slowest.probes),
        content_keys_read = index_gc.content_keys_read,
        client_records_drained,
        deadlines_restamped,
        cpu_user_ms = run_cpu.map(|cpu| millis(cpu.user)),
        cpu_system_ms = run_cpu.map(|cpu| millis(cpu.system)),
        failed_items,
        bodies_read = todo.queue_body_reads().saturating_sub(bodies_before),
        "queue-maintenance: run finished"
    );

    if failed_items > 0 {
        anyhow::bail!(
            "queue-maintenance: {failed_items} item(s) failed and were skipped (see warnings); \
             they will be retried on the next run"
        );
    }
    Ok(())
}

struct PhaseMark {
    at: Instant,
    http: HttpCounts,
    decode: DecodeCounts,
    cpu: Option<CpuTime>,
}

impl PhaseMark {
    /// The S3 peak in flight starts a new window.
    fn now() -> Self {
        Self {
            at: Instant::now(),
            http: S3_HTTP.checkpoint(),
            decode: S3_DECODE.checkpoint(),
            cpu: process_cpu_time(),
        }
    }
}

/// Each S3 client counts each attempt, so a retry adds a request.
fn log_phase_work(phase: &'static str, earlier: &PhaseMark, later: &PhaseMark) {
    let http = later.http.since(earlier.http);
    let decode = later.decode.since(earlier.decode);
    let cpu = later
        .cpu
        .zip(earlier.cpu)
        .map(|(later, earlier)| later.since(earlier));
    tracing::info!(
        phase,
        requests = http.requests,
        ok = http.ok,
        not_found = http.not_found,
        throttled = http.throttled,
        client_errors = http.client_errors,
        server_errors = http.server_errors,
        transport_errors = http.transport_errors,
        peak_in_flight = http.peak_in_flight,
        wait_ms = millis(http.wait),
        bodies_decoded = decode.bodies,
        stored_bytes = decode.stored_bytes,
        decoded_bytes = decode.decoded_bytes,
        decode_ms = millis(decode.time),
        cpu_ms = cpu.map(|cpu| millis(cpu.user + cpu.system)),
        "queue-maintenance: S3 and CPU work of a phase"
    );
}

#[derive(Debug, Clone, Copy)]
struct CpuTime {
    user: Duration,
    system: Duration,
}

impl CpuTime {
    fn since(self, earlier: CpuTime) -> CpuTime {
        CpuTime {
            user: self.user.saturating_sub(earlier.user),
            system: self.system.saturating_sub(earlier.system),
        }
    }
}

fn process_cpu_time() -> Option<CpuTime> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` writes one whole `rusage`, and `usage` has room for one.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: `getrusage` returned 0, so it wrote the whole struct.
    let usage = unsafe { usage.assume_init() };
    let duration = |time: libc::timeval| {
        Some(
            Duration::from_secs(u64::try_from(time.tv_sec).ok()?)
                + Duration::from_micros(u64::try_from(time.tv_usec).ok()?),
        )
    };
    Some(CpuTime {
        user: duration(usage.ru_utime)?,
        system: duration(usage.ru_stime)?,
    })
}

/// Settle every lease whose `lease_expiry` is in the past.
///
/// A lost lease is recorded under `lost/`, and a job that reaches
/// `lost_lease_limit` losses is retired as `lease_lost` (0 disables this), so a
/// job that crashes its device stops circulating.
///
/// Returns every listed lease's job (live for GC, since a concurrent recycle can
/// move it), the jobs resolved terminal here, and the failure count.
async fn settle_expired_leases(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    lost_lease_limit: usize,
    now: DateTime<Utc>,
) -> anyhow::Result<(HashSet<JobId>, HashSet<JobId>, usize)> {
    let mut seen = HashSet::new();
    let mut resolved = HashSet::new();
    let mut recycled = 0usize;
    let mut retired = 0usize;
    let mut failures = 0usize;
    for lease in todo.list_leases().await? {
        seen.insert(lease.job_id.clone());
        if lease.lease_expiry >= now {
            continue;
        }
        let (job_id, client_id, lease_expiry) =
            (&lease.job_id, &lease.client_id, lease.lease_expiry);
        // A failure leaves the lease for the next run: a wrong guess could re-run
        // a finished job and clobber its record.
        match settle_expired_lease(todo, submissions, catalog, &lease, lost_lease_limit, now).await
        {
            Ok(ExpiredLease::Settled(LeaseResolution::Recycled)) => {
                tracing::info!(
                    job_id = %job_id,
                    client_id = %client_id,
                    lease_expiry = %lease_expiry,
                    "queue-maintenance: recycled expired lease to avail/"
                );
                recycled += 1;
            }
            Ok(ExpiredLease::Settled(LeaseResolution::ResolvedStale)) => {
                tracing::info!(
                    job_id = %job_id,
                    client_id = %client_id,
                    lease_expiry = %lease_expiry,
                    "queue-maintenance: job already has a submission record; deleted stale lease"
                );
                resolved.insert(job_id.clone());
            }
            Ok(ExpiredLease::Retired { losses, recorded }) => {
                if recorded {
                    tracing::warn!(
                        job_id = %job_id,
                        client_id = %client_id,
                        losses,
                        "queue-maintenance: retired a job whose lease was lost {losses} times"
                    );
                    retired += 1;
                }
                resolved.insert(job_id.clone());
            }
            Ok(ExpiredLease::Settled(LeaseResolution::Gone)) => {
                tracing::debug!(job_id = %job_id, client_id = %client_id, "expired lease already resolved elsewhere");
            }
            Err(e) => {
                tracing::warn!(job_id = %job_id, client_id = %client_id, error = %e, "failed to resolve expired lease");
                failures += 1;
            }
        }
    }
    if recycled + retired > 0 {
        tracing::info!(
            recycled,
            retired,
            "queue-maintenance: expired-lease pass done"
        );
    }
    Ok((seen, resolved, failures))
}

#[derive(Debug)]
pub enum ExpiredLease {
    Settled(LeaseResolution),
    /// `recorded` is false when a result landed first and was kept.
    Retired {
        losses: usize,
        recorded: bool,
    },
}

/// Settle one expired lease; see [`settle_expired_leases`].
///
/// A holder that renews its lapsed lease just before a retirement keeps a lease
/// on a recorded job; a later run deletes it once it lapses.
pub async fn settle_expired_lease(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    lease: &Lease,
    lost_lease_limit: usize,
    now: DateTime<Utc>,
) -> anyhow::Result<ExpiredLease> {
    let Lease {
        job_id,
        expires_at,
        client_id,
        lease_expiry,
    } = lease;
    if lost_lease_limit > 0 && !submissions.has_record(job_id).await? {
        todo.write_lost_lease(job_id, client_id, *lease_expiry)
            .await?;
        let losses = todo.list_lost_leases(job_id).await?;
        if losses.len() >= lost_lease_limit {
            let Some(body) = todo
                .get_leased(job_id, *expires_at, client_id, *lease_expiry)
                .await?
            else {
                return Ok(ExpiredLease::Settled(LeaseResolution::Gone));
            };
            let recorded = resolve_job_terminal(
                todo,
                submissions,
                catalog,
                &QueueEntry::Lease(lease),
                &body,
                std::future::ready((lost_lease_reason(&losses), FailureCode::Unspecified)),
                "lease_lost",
                FailureSource::LeaseLost,
                now,
            )
            .await?;
            return Ok(ExpiredLease::Retired {
                losses: losses.len(),
                recorded,
            });
        }
    }
    resolve_or_recycle_lease(
        todo,
        submissions,
        job_id,
        *expires_at,
        client_id,
        *lease_expiry,
    )
    .await
    .map(ExpiredLease::Settled)
}

/// The `failed_reason` of a `lease_lost` retirement.
fn lost_lease_reason(losses: &[(ClientId, DateTime<Utc>)]) -> String {
    let mut clients: Vec<&str> = losses.iter().map(|(c, _)| c.as_str()).collect();
    clients.sort_unstable();
    clients.dedup();
    format!(
        "Lease lost without a result {} times (clients: {})",
        losses.len(),
        clients.join(", ")
    )
}

/// Outcome of [`resolve_or_recycle_lease`] for one lease.
#[derive(Debug)]
pub enum LeaseResolution {
    Recycled,
    /// The job has a record, so the lease was deleted and the job is terminal.
    ResolvedStale,
    /// Another actor resolved the lease first. Benign.
    Gone,
}

/// Resolve one lease that should no longer be held: recycle its job back to
/// `avail/`, unless the job has a record. Then the lease is a leftover of the
/// submit path's best-effort teardown and is deleted, since recycling would make
/// a finished job claimable again.
pub async fn resolve_or_recycle_lease(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    job_id: &JobId,
    expires_at: ExpiresAt,
    client_id: &ClientId,
    lease_expiry: DateTime<Utc>,
) -> anyhow::Result<LeaseResolution> {
    if submissions.has_record(job_id).await? {
        todo.delete_lease(job_id, expires_at, client_id, lease_expiry)
            .await?;
        return Ok(LeaseResolution::ResolvedStale);
    }
    match todo
        .recycle_lease(job_id, expires_at, client_id, lease_expiry)
        .await?
    {
        RecycleResult::Recycled => Ok(LeaseResolution::Recycled),
        RecycleResult::Gone => Ok(LeaseResolution::Gone),
    }
}

/// Settle every job in `all_keys` past the `expires_at` its key encodes:
/// re-date it onto the earliest horizon a member plan still holds, or retire it.
///
/// Only lapsed keys are moved, which the claim gate refuses, so a claim races a
/// re-date only under clock skew. A `Never` key is never settled, so such a job
/// outlives every member plan's horizon.
///
/// Returns the surviving entries (re-dated ones under their new keys), the jobs
/// positively removed, and the failure count.
async fn expire_overdue_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    plans: &dyn PlanStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailEntry>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailEntry>, HashSet<JobId>, usize)> {
    let mut survivors = Vec::with_capacity(all_keys.len());
    let mut removed = HashSet::new();
    let mut failures = 0usize;
    for avail in all_keys {
        // `is_expired` is the claim gates' predicate; the pattern only binds
        // the deadline for the failure reason.
        let deadline = match avail.expires_at {
            ExpiresAt::At(deadline) if avail.expires_at.is_expired(now) => deadline,
            ExpiresAt::At(_) | ExpiresAt::Never => {
                survivors.push(avail);
                continue;
            }
        };
        match settle_overdue_job(todo, submissions, plans, catalog, &avail, deadline, now).await {
            Ok(Overdue::Redated(redated)) => survivors.push(redated),
            Ok(Overdue::Retired) => {
                removed.insert(avail.job_id.clone());
            }
            // Left the queue by another path, whose delete took its markers.
            Ok(Overdue::Vanished) => {}
            Err(e) => {
                tracing::warn!(job_id = %avail.job_id, error = %e, "failed to settle overdue job");
                failures += 1;
                survivors.push(avail);
            }
        }
    }
    Ok((survivors, removed, failures))
}

/// What [`settle_overdue_job`] did with one job past its encoded deadline.
enum Overdue {
    /// Carries the entry under its new deadline.
    Redated(AvailEntry),
    Retired,
    Vanished,
}

/// Settle one job whose encoded deadline has lapsed: re-date it onto the earliest
/// horizon its member plans still hold, or retire it when none remains. A plan's
/// horizon is the latest of its markers, as [`TodoStore::restamp_avail_deadlines`]
/// reads it.
///
/// Needs no record guard of its own: claim and submission contend for one lease
/// file. An entry a failed expiry left has no unlapsed marker, so it reaches
/// the retire arm, whose `resolve_job_terminal` guard keeps the record; one
/// another retirement left is at worst re-dated, and its own pass retries it.
async fn settle_overdue_job(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    plans: &dyn PlanStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailEntry,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<Overdue> {
    // Fresh, since the lapse pass just deleted markers. An error must propagate:
    // read as "no member", it would retire a job another plan still waits on.
    let horizons: HashMap<PlanId, ExpiresAt> = plans
        .list_job_plan_markers(&avail.job_id)
        .await?
        .into_iter()
        .filter(|(_, horizon)| !horizon.is_expired(now))
        .fold(HashMap::new(), |mut acc, (plan_id, horizon)| {
            acc.entry(plan_id)
                .and_modify(|latest| *latest = (*latest).max(horizon))
                .or_insert(horizon);
            acc
        });
    let Some(to) = horizons.into_values().min() else {
        return match expire_one_job(todo, submissions, catalog, avail, deadline, now).await? {
            true => Ok(Overdue::Retired),
            false => Ok(Overdue::Vanished),
        };
    };
    match todo
        .redate_avail(&avail.job_id, avail.expires_at, to)
        .await?
    {
        RedateResult::Redated => {
            // The re-date sets the job's reindex flag.
            tracing::info!(
                job_id = %avail.job_id,
                from = %avail.expires_at,
                to = %to,
                "queue-maintenance: re-dated a job to the earliest horizon its member plans hold"
            );
            Ok(Overdue::Redated(AvailEntry {
                job_id: avail.job_id.clone(),
                expires_at: to,
            }))
        }
        // Claimed first; the lease recycles under the old key for a later run.
        RedateResult::Gone => Ok(Overdue::Vanished),
    }
}

/// Retire each flagged `avail/` job that no possible client can match.
/// Record a synthetic `"system"` failure.
/// Ingestion rejects these jobs. Another writer or an older build created them.
///
/// This pass does not check an indexed job again. A build that makes
/// these rules stricter must ship a migration that flags every available job.
///
/// Not part of [`escalate_all_denied_jobs`]: such a job collects no denials. A
/// job an older build indexed can be claimed while it is retired, leaving a
/// failure and a live lease, until `flag-reindex --all` runs.
async fn retire_unmatchable_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailEntry>,
    flagged: &HashSet<JobId>,
    bodies: &mut HashMap<AvailEntry, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailEntry>, HashSet<JobId>, HashSet<JobId>)> {
    let mut survivors = Vec::with_capacity(all_keys.len());
    let mut removed = HashSet::new();
    let mut failures = HashSet::new();
    let mut checked = 0usize;
    for avail in all_keys {
        if !flagged.contains(&avail.job_id) {
            survivors.push(avail);
            continue;
        }
        checked += 1;
        match retire_one_if_unmatchable(todo, submissions, catalog, &avail, bodies, now).await {
            Ok(true) => {
                removed.insert(avail.job_id.clone());
            }
            Ok(false) => survivors.push(avail),
            Err(e) => {
                tracing::warn!(job_id = %avail.job_id, error = %e, "failed to retire unmatchable job");
                failures.insert(avail.job_id.clone());
                survivors.push(avail);
            }
        }
    }
    tracing::info!(
        jobs_checked_for_retirement = checked,
        jobs_retired = removed.len(),
        "queue-maintenance: checked flagged jobs for retirement"
    );
    Ok((survivors, removed, failures))
}

/// Returns `true` when the job was positively removed.
///
/// A vacuous exclusion group fails open, so it earns a warning and no
/// retirement: only the author knows which clients were meant.
async fn retire_one_if_unmatchable(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailEntry,
    bodies: &mut HashMap<AvailEntry, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(body) = cached_body(todo, avail, bodies).await? else {
        return Ok(false);
    };
    let (cause, rule) = if capability_clauses_malformed(&body) {
        (
            "malformed_clauses",
            "an eligibility clause is malformed, so the job fails closed for every client"
                .to_owned(),
        )
    } else if let Some(rule) = matching::unmatchable_reason(&body) {
        ("unmatchable", rule)
    } else {
        if let Some(namespace) = plan_ingestion::vacuous_exclusion_namespace(&body) {
            tracing::warn!(
                job_id = %avail.job_id,
                namespace,
                "queue-maintenance: queued job carries a `not_all_of` group no client can \
                 carry whole, so the group excludes nobody and the job runs wider than \
                 its author wrote; ingestion refuses such a body"
            );
        }
        return Ok(false);
    };

    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        &QueueEntry::Avail(avail),
        &body,
        std::future::ready((
            format!("No client can ever be eligible for this job: {rule}"),
            FailureCode::Unspecified,
        )),
        cause,
        FailureSource::Unmatchable,
        now,
    )
    .await?;
    // Names the rule so the author can fix the clause.
    tracing::info!(
        job_id = %avail.job_id,
        cause,
        rule = %rule,
        "queue-maintenance: retired a job no client can ever match"
    );
    Ok(true)
}

/// Check each flagged `avail/` job with a closed `clients` roster.
/// If every client denied the job, record a synthetic `"system"` failure.
/// This function owns the rule. The submit path records the denial but does not
/// escalate it. Without this function, a job with a denied roster stays in `avail/`.
///
/// A vanished entry survives: unlike an expired one it was likely just claimed.
async fn escalate_all_denied_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailEntry>,
    flagged: &HashSet<JobId>,
    bodies: &mut HashMap<AvailEntry, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailEntry>, HashSet<JobId>, HashSet<JobId>)> {
    let denied_by: HashMap<JobId, HashSet<ClientId>> = todo
        .list_all_denied()
        .await?
        .into_iter()
        .fold(HashMap::new(), |mut acc, (job_id, client_id)| {
            acc.entry(job_id).or_default().insert(client_id);
            acc
        });

    let mut survivors = Vec::with_capacity(all_keys.len());
    let mut removed = HashSet::new();
    let mut failures = HashSet::new();
    let mut checked = 0;
    for avail in all_keys {
        if !flagged.contains(&avail.job_id) {
            survivors.push(avail);
            continue;
        }
        let Some(denied) = denied_by.get(&avail.job_id) else {
            survivors.push(avail);
            continue;
        };
        checked += 1;
        match escalate_one_if_all_denied(todo, submissions, catalog, &avail, denied, bodies, now)
            .await
        {
            Ok(true) => {
                removed.insert(avail.job_id.clone());
            }
            Ok(false) => survivors.push(avail),
            Err(e) => {
                tracing::warn!(job_id = %avail.job_id, error = %e, "failed to escalate all-denied job");
                failures.insert(avail.job_id.clone());
                survivors.push(avail);
            }
        }
    }
    tracing::info!(
        jobs_checked_for_escalation = checked,
        jobs_escalated = removed.len(),
        "queue-maintenance: checked flagged jobs with a denial for escalation"
    );
    Ok((survivors, removed, failures))
}

/// Fetch `avail`'s job body through the run's shared cache. `Ok(None)` means
/// the entry left `avail/` since the listing.
pub(crate) async fn cached_body(
    todo: &dyn TodoStore,
    avail: &AvailEntry,
    bodies: &mut HashMap<AvailEntry, Value>,
) -> anyhow::Result<Option<Value>> {
    if let Some(cached) = bodies.get(avail) {
        return Ok(Some(cached.clone()));
    }
    let Some(body) = todo.get_avail(&avail.job_id, avail.expires_at).await? else {
        return Ok(None);
    };
    bodies.insert(avail.clone(), body.clone());
    Ok(Some(body))
}

/// Returns `true` when the job was positively removed.
async fn escalate_one_if_all_denied(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailEntry,
    denied: &HashSet<ClientId>,
    bodies: &mut HashMap<AvailEntry, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(body) = cached_body(todo, avail, bodies).await? else {
        return Ok(false);
    };
    let denied_ids: HashSet<&str> = denied.iter().map(ClientId::as_str).collect();
    let Some(roster) = pipette_core::matching::clients_only_roster(&body)
        .filter(|roster| roster.iter().all(|c| denied_ids.contains(c)))
    else {
        return Ok(false);
    };

    // From the denied set, so an id listed twice counts once.
    let members = denied.iter().filter(|d| roster.contains(&d.as_str()));
    let failure = async {
        let denials: Vec<DeniedClient> =
            stream::iter(members.map(|id| denied_client(todo, &avail.job_id, id)))
                .buffered(STORAGE_CONCURRENCY)
                .collect()
                .await;
        (
            summarize_denials(&denials),
            prevailing_failure_code(&denials),
        )
    };

    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        &QueueEntry::Avail(avail),
        &body,
        failure,
        "all_denied",
        FailureSource::AllClientsDenied,
        now,
    )
    .await?;
    Ok(true)
}

/// One roster member's stored denial. A failed read does not stop the
/// escalation: a non-JSON marker would otherwise keep the job forever.
async fn denied_client(todo: &dyn TodoStore, job_id: &JobId, client_id: &ClientId) -> DeniedClient {
    let record = todo
        .get_denied(job_id, client_id)
        .await
        .inspect_err(|e| {
            tracing::warn!(
                job_id = %job_id,
                client_id = %client_id,
                error = %format_args!("{e:#}"),
                "all-denied summary: unreadable denied marker, reason omitted"
            );
        })
        .ok()
        .map(Option::unwrap_or_default);
    DeniedClient {
        client_id: client_id.clone(),
        record,
    }
}

enum QueueEntry<'a> {
    Avail(&'a AvailEntry),
    Lease(&'a Lease),
}

impl QueueEntry<'_> {
    fn job_id(&self) -> &JobId {
        match self {
            Self::Avail(avail) => &avail.job_id,
            Self::Lease(lease) => &lease.job_id,
        }
    }

    fn expires_at(&self) -> ExpiresAt {
        match self {
            Self::Avail(avail) => avail.expires_at,
            Self::Lease(lease) => lease.expires_at,
        }
    }

    async fn delete(&self, todo: &dyn TodoStore) -> anyhow::Result<()> {
        match self {
            // By job: a re-stamp may have moved the deadline since the read.
            Self::Avail(avail) => todo.delete_avail_by_job(&avail.job_id).await,
            Self::Lease(lease) => {
                todo.delete_lease(
                    &lease.job_id,
                    lease.expires_at,
                    &lease.client_id,
                    lease.lease_expiry,
                )
                .await
            }
        }
    }
}

/// Positively resolve a queue entry as terminal: write a synthetic `"system"`
/// failure, then delete the entry. Returns whether it wrote the failure.
///
/// An existing record is kept. The `has_record` guard is not atomic with the
/// write, so the write is also exclusive. Record before delete, because a joiner
/// checks the queue then the record. Markers are left to the age-gated GC: a
/// joiner's marker may already stand.
///
/// `failure_reason` is awaited only when the record is written.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds"
)]
async fn resolve_job_terminal(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    entry: &QueueEntry<'_>,
    body: &Value,
    failure: impl Future<Output = (String, FailureCode)>,
    cause: &str,
    failure_source: FailureSource,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    if submissions.has_record(entry.job_id()).await? {
        entry.delete(todo).await?;
        tracing::info!(
            job_id = %entry.job_id(),
            cause,
            "queue-maintenance: job already has a submission record; removed the leftover queue entry"
        );
        return Ok(false);
    }
    let (failure_reason, failure_code) = failure.await;
    match record_system_failure(
        submissions,
        catalog,
        body,
        failure_reason,
        failure_code,
        failure_source,
        now,
    )
    .await
    {
        Ok(CreateOutcome::Created) => {}
        // A submission landed after the guard; it wins, and the entry still goes.
        Ok(CreateOutcome::AlreadyExists) => {
            entry.delete(todo).await?;
            tracing::info!(
                job_id = %entry.job_id(),
                cause,
                "queue-maintenance: a submission landed while the job was being retired; kept it \
                 and removed the leftover queue entry"
            );
            return Ok(false);
        }
        // A store blip or a restorable catalog entry is worth another run.
        Err(e) if !e.chain().any(|c| c.is::<UnrecordableJobError>()) => return Err(e),
        // Permanent, so drop it. Such a body belongs to no plan, so no manifest
        // waits on its outcome.
        Err(e) => {
            entry.delete(todo).await?;
            tracing::warn!(
                job_id = %entry.job_id(),
                expires_at = %entry.expires_at(),
                cause,
                error = %e,
                "queue-maintenance: deleted a job whose body can never produce a failure \
                 record — no retry could succeed; ingestion refuses such a body, so it was \
                 written straight into avail/"
            );
            return Ok(false);
        }
    }
    entry.delete(todo).await?;
    tracing::info!(
        job_id = %entry.job_id(),
        expires_at = %entry.expires_at(),
        cause,
        "queue-maintenance: wrote synthetic system failure and removed job"
    );
    Ok(true)
}

/// Returns `true` when the job was positively removed, `false` when the entry
/// had already vanished.
async fn expire_one_job(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailEntry,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(body) = todo.get_avail(&avail.job_id, avail.expires_at).await? else {
        return Ok(false);
    };
    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        &QueueEntry::Avail(avail),
        &body,
        std::future::ready((
            format!(
                "Job expired at {} before any client completed it",
                deadline.to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            FailureCode::Unspecified,
        )),
        "expired",
        FailureSource::Expired,
        now,
    )
    .await?;
    Ok(true)
}

/// Fail a queued job unrun because `smaller`, a smaller run of its family that
/// ran `benchmark_id`, failed with `code`. Returns whether it wrote the
/// failure. A leased job keeps running: the delete takes only an unleased row.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds"
)]
pub async fn fail_larger_run(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailEntry,
    body: &Value,
    smaller: &JobId,
    benchmark_id: &BenchmarkId,
    code: FailureCode,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        &QueueEntry::Avail(avail),
        body,
        std::future::ready((
            format!("not run: {benchmark_id} of this model failed with {code} in {smaller}"),
            code,
        )),
        "smaller_run_failed",
        FailureSource::SmallerRunFailed,
        now,
    )
    .await
}

/// The body-cache key of a flagged job: its entry at the read.
fn reindex_entry(job: &ReindexJob) -> AvailEntry {
    AvailEntry {
        job_id: job.job_id.clone(),
        expires_at: job.expires_at,
    }
}

/// Make the eligible index match the workers. Returns the joiners, for a full
/// rebuild this run; a leaver loses its eligible partition.
///
/// Not a `pending-reindex/` flag for joiners: it would refuse the claims and
/// submissions of a joiner that holds a live lease.
async fn track_worker_changes(
    todo: &dyn TodoStore,
    indexed: &HashSet<ClientId>,
    workers: &HashSet<ClientId>,
) -> anyhow::Result<BTreeSet<ClientId>> {
    if indexed == workers {
        return Ok(BTreeSet::new());
    }
    let joined: BTreeSet<ClientId> = workers.difference(indexed).cloned().collect();
    joined.iter().for_each(|client_id| {
        tracing::info!(
            client_id = %client_id,
            "queue-maintenance: client joined the workers; rebuilding its markers this run"
        );
    });
    // Sorted so a failing write stops at the same client on every run.
    let mut left: Vec<&ClientId> = indexed.difference(workers).collect();
    left.sort_unstable();
    for client_id in left {
        todo.delete_eligible_for_client(client_id).await?;
        tracing::info!(
            client_id = %client_id,
            "queue-maintenance: cleared eligible markers for a client that left the workers"
        );
    }
    Ok(joined)
}

/// Jobs whose memberships all point at creating or missing plans. A job with
/// no membership keeps its existing queue behavior.
fn uncommitted_jobs(
    jobs: &HashSet<JobId>,
    markers: &[JobPlanMarker],
    manifests: &[PlanManifest],
) -> HashSet<JobId> {
    let statuses: HashMap<&PlanId, PlanStatus> = manifests
        .iter()
        .map(|plan| (&plan.plan_id, plan.status))
        .collect();
    let committed: HashSet<&JobId> = markers
        .iter()
        .filter(|marker| {
            statuses
                .get(&marker.plan_id)
                .is_some_and(|status| *status != PlanStatus::Creating)
        })
        .map(|marker| &marker.job_id)
        .collect();
    let has_membership: HashSet<&JobId> = markers.iter().map(|marker| &marker.job_id).collect();
    jobs.iter()
        .filter(|job_id| has_membership.contains(job_id) && !committed.contains(job_id))
        .cloned()
        .collect()
}

/// Rebuild eligibility for the fetched flagged jobs against the workers.
/// Clear their flags. The caller excludes jobs that wait for a committed plan
/// and jobs whose terminal check failed. The flags of those jobs stay set.
///
/// A job that changes between the read and the write keeps its flag too; the
/// next run indexes it again.
async fn index_flagged_jobs(
    todo: &dyn TodoStore,
    workers: &[&Client],
    tags: &FleetTags,
    flagged: &[ReindexJob],
) -> anyhow::Result<HashSet<JobId>> {
    let mut indexed_ids = HashSet::new();
    let (mut indexed, mut rows, mut moved) = (0usize, 0usize, 0usize);
    for job in flagged {
        let matching: Vec<ClientId> = workers
            .iter()
            .filter(|client| job_matches_client(&job.body, client, tags))
            .map(|client| client.client_id.clone())
            .collect();
        if todo
            .replace_eligibility(&job.job_id, job.revision, &matching)
            .await?
        {
            indexed_ids.insert(job.job_id.clone());
            indexed += 1;
            rows += matching.len();
        } else {
            moved += 1;
            tracing::debug!(
                job_id = %job.job_id,
                "queue-maintenance: kept the reindex flag of a job that changed after the read"
            );
        }
    }
    tracing::info!(
        jobs_indexed = indexed,
        rows_written = rows,
        jobs_skipped_moved = moved,
        "queue-maintenance: indexed flagged jobs"
    );
    Ok(indexed_ids)
}

/// Re-evaluate each flagged client, and each joiner, against the whole `avail/`
/// set; a flagged non-worker gets its markers cleared instead.
///
/// The function reads the current record and deletes only the earlier flags. It
/// preserves newer reindex requests. Leased jobs get their reindex
/// flag before the wipe, so a crash never loses one. Returns joiners whose
/// rebuild did not pass its recheck, for the caller to retry next run.
async fn reindex_flagged_clients(
    todo: &dyn TodoStore,
    auth: &dyn ClientStore,
    tag_store: &dyn TagStore,
    workers: &HashSet<ClientId>,
    joined: &BTreeSet<ClientId>,
    listed: &[JobId],
    leased_ids: &HashSet<JobId>,
) -> anyhow::Result<HashSet<ClientId>> {
    let flags = todo.list_pending_reindex().await?;
    let mut retry_joiners = HashSet::new();
    // One rebuild consumes all of a client's flags; a joiner has none.
    let by_client: BTreeMap<ClientId, Vec<ReindexRequest>> = flags.into_iter().fold(
        joined.iter().map(|c| (c.clone(), Vec::new())).collect(),
        |mut m, request| {
            m.entry(request.client_id.clone())
                .or_default()
                .push(request);
            m
        },
    );
    if by_client.is_empty() {
        return Ok(retry_joiners);
    }

    // Only a rebuild misses the leased jobs; a non-worker's cleanup drops its
    // whole partition, leased jobs included.
    let groups = if by_client
        .keys()
        .any(|client_id| workers.contains(client_id))
    {
        let leased: Vec<JobId> = leased_ids.iter().cloned().collect();
        todo.flag_for_reindex(&leased).await?;
        if !leased_ids.is_empty() {
            tracing::info!(
                jobs_flagged = leased_ids.len(),
                "queue-maintenance: flagged leased jobs for deferred reindex"
            );
        }
        let groups = todo.avail_clause_groups(listed).await?;
        let present: HashSet<&JobId> = groups.iter().flat_map(|(_, jobs)| jobs).collect();
        let missing: Vec<JobId> = listed
            .iter()
            .filter(|id| !present.contains(id))
            .cloned()
            .collect();
        todo.flag_for_reindex(&missing).await?;
        // A claim since the listing, or a body whose matcher projection is NULL.
        if !missing.is_empty() {
            tracing::warn!(
                jobs_flagged = missing.len(),
                "queue-maintenance: flagged listed jobs that a client rebuild could not group"
            );
        }
        groups
    } else {
        Vec::new()
    };

    for (client_id, requests) in &by_client {
        // `claim` refuses a non-worker, so its markers are dead; a later join
        // rebuilds them.
        let is_worker = workers.contains(client_id);
        let record = if is_worker {
            auth.get_client(client_id).await?
        } else {
            None
        };
        let Some(client) = record else {
            if joined.contains(client_id) {
                retry_joiners.insert(client_id.clone());
            }
            // Only the captured flags: a racing re-registration's flag survives.
            todo.delete_eligible_for_client(client_id).await?;
            for request in requests {
                todo.delete_pending_reindex(request).await?;
            }
            tracing::info!(
                client_id = %client_id,
                reason = if is_worker { "deleted" } else { "not a worker" },
                "queue-maintenance: cleared eligible markers for a flagged client without a rebuild"
            );
            continue;
        };

        // Fresh, not the run's snapshot: a tag write flags its client, so this
        // rebuild must see it.
        let tags = client_tags(tag_store, client_id).await?;
        let evaluated_capabilities = tags.capabilities_of(&client);

        todo.delete_eligible_for_client(client_id).await?;
        let matching: Vec<JobId> = groups
            .iter()
            .filter(|(clauses, _)| job_matches_client(clauses, &client, &tags))
            .flat_map(|(_, jobs)| jobs.iter().cloned())
            .collect();
        todo.write_eligible_rows(client_id, &matching).await?;
        tracing::info!(
            client_id = %client_id,
            markers_written = matching.len(),
            clause_sets = groups.len(),
            "queue-maintenance: reindexed client after a matching-input change"
        );

        // Preserve retry state if the record changed mid-rebuild: a `PATCH`
        // whose flag write failed has no other repair trigger. A failed read
        // also requires another rebuild.
        let fresh = async {
            let record = auth.get_client(client_id).await?;
            let tags = client_tags(tag_store, client_id).await?;
            anyhow::Ok(record.map(|record| tags.capabilities_of(&record)))
        }
        .await;
        let record_unchanged = match fresh {
            Ok(fresh) => fresh.is_some_and(|fresh| fresh == evaluated_capabilities),
            Err(e) => {
                tracing::warn!(
                    client_id = %client_id,
                    error = %e,
                    "queue-maintenance: recheck read failed; retaining this client's retry state"
                );
                false
            }
        };
        if !record_unchanged {
            if joined.contains(client_id) {
                retry_joiners.insert(client_id.clone());
            }
            tracing::info!(
                client_id = %client_id,
                flags_retained = requests.len(),
                joiner_retry = joined.contains(client_id),
                "queue-maintenance: client's rebuild recheck did not pass; retaining retry state for the next run"
            );
            continue;
        }
        for request in requests {
            todo.delete_pending_reindex(request).await?;
        }
    }
    Ok(retry_joiners)
}

/// Whether to delete an orphaned key now: the last run staged it. The function
/// adds a key to `next` on its first sighting.
pub(crate) fn confirm_or_stage(
    prior: &HashSet<String>,
    next: &mut HashSet<String>,
    key: String,
) -> bool {
    if prior.contains(&key) {
        true
    } else {
        next.insert(key);
        false
    }
}

/// The `eligible/` markers that a claim at `now` could take, by the claim
/// path's candidate filters.
fn claimable_markers<'a>(
    eligible: &'a [(ClientId, JobId, ExpiresAt)],
    avail_expiries: &HashMap<JobId, ExpiresAt>,
    denied: &[(JobId, ClientId)],
    now: DateTime<Utc>,
) -> Vec<&'a (ClientId, JobId, ExpiresAt)> {
    let denied: HashSet<(&JobId, &ClientId)> = denied.iter().map(|(j, c)| (j, c)).collect();
    eligible
        .iter()
        .filter(|(client_id, job_id, expires_at)| {
            avail_expiries.get(job_id) == Some(expires_at)
                && !expires_at.is_expired(now)
                && !denied.contains(&(job_id, client_id))
        })
        .collect()
}

pub(crate) fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Delete the staged job bodies older than `max_age`. A crashed planner leaves
/// them.
async fn cleanup_stale_tmp(todo: &dyn TodoStore, max_age: Duration) -> anyhow::Result<()> {
    let deleted = todo.delete_stale_tmp(max_age).await?;
    if deleted > 0 {
        tracing::info!(
            files_deleted = deleted,
            "queue-maintenance: deleted stale tmp/ files"
        );
    }
    Ok(())
}

pub async fn client_tags(
    tag_store: &dyn TagStore,
    client_id: &ClientId,
) -> anyhow::Result<FleetTags> {
    let tags = tag_store.get_client_tags(client_id).await?;
    Ok(FleetTags::new(
        tags.into_iter().map(|tag| (client_id.clone(), tag)),
    ))
}

#[cfg(test)]
mod tests {
    use futures::stream::TryStreamExt;
    use rstest::rstest;

    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_core::config::StorageConfig;
    use pipette_core::denial::DenialRecord;
    use pipette_plan_types::ClientId;
    use pipette_store::ClientStore;
    use pipette_store_fs::{LocalFsSubmissionStore, LocalFsTagStore};
    use pipette_store_sql::scratch::TestDb;
    use pipette_store_testkit::seed_leased_until;

    use super::*;

    /// A database queue, for a test of a pass that reads or writes the
    /// eligibility index. `None` skips the test without `TEST_DATABASE_URL`.
    async fn sql_todo() -> anyhow::Result<Option<(std::sync::Arc<dyn TodoStore>, TestDb)>> {
        let Some(db) = TestDb::new().await? else {
            return Ok(None);
        };
        let todo = pipette_store_sql::SqlTodoStore::new(db.db().clone());
        Ok(Some((std::sync::Arc::new(todo), db)))
    }

    /// Lease a new job whose reindex flag is clear, as after an index pass.
    async fn seed_indexed_lease(todo: &dyn TodoStore, job_id: &str) -> anyhow::Result<JobId> {
        let job = pipette_store_testkit::seed_avail(
            todo,
            job_id,
            ExpiresAt::Never,
            &serde_json::json!({"job_id": job_id}),
        )
        .await?;
        for flagged in todo
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs
        {
            todo.replace_eligibility(&flagged.job_id, flagged.revision, &[])
                .await?;
        }
        let lease_expiry = Utc::now() + chrono::Duration::hours(1);
        todo.claim_job(
            &job,
            ExpiresAt::Never,
            &ClientId::try_new("holder")?,
            lease_expiry,
        )
        .await?;
        Ok(job)
    }

    #[tokio::test]
    async fn sql_queue_runs_every_maintenance_pass() -> anyhow::Result<()> {
        use pipette_store_testkit::seed_avail;
        let Some(db) = TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let stores = db.stores(pipette_store_fs::build_local_fs_stores(
            &pipette_core::config::Config {
                storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                ..Default::default()
            },
            pipette_store_testkit::no_queue(),
        )?);
        let worker = settle_test_client("sql-worker", "66", "chip-a")?;
        let client = &worker.client_id;
        stores.clients.put_client(&worker).await?;
        stores
            .clients
            .put_client_permissions(
                client,
                &pipette_core::client_permission::ClientPermissionSet::from_iter([
                    pipette_plan_types::ClientPermission::PerformWork,
                ]),
            )
            .await?;
        // Mark the worker as indexed, so the run does not rebuild its rows.
        stores
            .todo
            .write_workers(&HashSet::from([client.clone()]))
            .await?;
        let body =
            |id: &str| pipette_store_testkit::todo_job_body(id, &[client.as_str()], "bench-1");
        let todo = &*stores.todo;
        let available = seed_avail(
            todo,
            "sql-available",
            ExpiresAt::Never,
            &body("sql-available"),
        )
        .await?;
        let past = Utc::now() - chrono::Duration::hours(1);
        let recycled = seed_leased_until(
            todo,
            "sql-recycled",
            ExpiresAt::Never,
            client,
            past,
            &body("sql-recycled"),
        )
        .await?;
        let overdue = seed_avail(
            todo,
            "sql-overdue",
            ExpiresAt::At(past),
            &body("sql-overdue"),
        )
        .await?;
        let catalog = HashMap::from([(
            BenchmarkId::try_new("bench-1")?,
            Benchmark::from_toml(
                "bench-1",
                "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 100",
            )?,
        )]);
        stream::iter(0..2)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|_| async {
                run(
                    todo,
                    &*stores.clients,
                    &*stores.tags,
                    &*stores.submissions,
                    &*stores.plans,
                    &catalog,
                    MaintenanceSettings {
                        tmp_max: Duration::from_secs(86_400),
                        plan_creating_max: Duration::from_secs(21_600),
                        lost_lease_limit: 3,
                    },
                )
                .await?;
                assert!(
                    todo.list_eligible_for_client(client)
                        .await?
                        .contains(&(available.clone(), ExpiresAt::Never))
                );
                assert!(todo.get_avail_by_job(&recycled).await?.is_some());
                assert_eq!(todo.list_lost_leases(&recycled).await?.len(), 1);
                assert!(todo.get_avail_by_job(&overdue).await?.is_none());
                assert_eq!(todo.backlog().await?.get(client), Some(&2));
                assert_eq!(
                    todo.campaign_backlog(client, Utc::now())
                        .await?
                        .outside_campaigns,
                    2
                );
                anyhow::Ok(())
            })
            .await?;
        Ok(())
    }

    /// An expired lease whose job has a record is deleted, not recycled, and
    /// reported as resolved.
    #[tokio::test]
    async fn recycle_pass_reports_live_and_expired_leases_as_seen() -> anyhow::Result<()> {
        let Some((todo, _db)) = sql_todo().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));

        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let client = ClientId::try_new("client1")?;
        let todo_ref = &*todo;
        let client_ref = &client;
        stream::iter(
            [
                ("job-expired", "2026-01-01T00:00:00Z"),
                ("job-live", "2026-12-31T00:00:00Z"),
                ("job-recorded", "2026-01-01T00:00:00Z"),
            ]
            .map(Ok::<_, anyhow::Error>),
        )
        .try_for_each(|(job, expiry)| async move {
            seed_lease(
                todo_ref,
                &JobId::new_unchecked(job),
                client_ref,
                expiry.parse()?,
            )
            .await
        })
        .await?;
        let recorded = JobId::new_unchecked("job-recorded");
        submissions
            .write_processed(
                &recorded,
                &serde_json::json!({ "job_id": "job-recorded", "message_type": "success" }),
            )
            .await?;

        let (seen, resolved, failures) =
            settle_expired_leases(&*todo, &submissions, &HashMap::new(), 3, now).await?;

        assert_eq!(failures, 0);
        assert!(seen.contains(&JobId::new_unchecked("job-expired")));
        assert!(seen.contains(&JobId::new_unchecked("job-live")));
        assert!(seen.contains(&recorded));
        assert_eq!(resolved, HashSet::from([recorded.clone()]));
        assert!(
            todo.get_avail_by_job(&JobId::new_unchecked("job-expired"))
                .await?
                .is_some()
        );
        assert!(todo.get_avail_by_job(&recorded).await?.is_none());
        assert_eq!(todo.list_leases().await?.len(), 1);
        Ok(())
    }

    /// A record can be tens of megabytes. The counters are process-wide, so
    /// this needs its own process, as `cargo nextest` gives it.
    #[tokio::test]
    async fn recycle_pass_decodes_no_submission_body() -> anyhow::Result<()> {
        let Some((todo, _db)) = sql_todo().await? else {
            return Ok(());
        };
        let submissions = pipette_store_s3::in_memory_submission_store();

        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let client = ClientId::try_new("client1")?;
        let todo_ref = &*todo;
        let client_ref = &client;
        stream::iter(["job-unrecorded", "job-recorded"].map(Ok::<_, anyhow::Error>))
            .try_for_each(|job| async move {
                seed_lease(
                    todo_ref,
                    &JobId::new_unchecked(job),
                    client_ref,
                    "2026-01-01T00:00:00Z".parse()?,
                )
                .await
            })
            .await?;
        let recorded = JobId::new_unchecked("job-recorded");
        submissions
            .write_processed(
                &recorded,
                &serde_json::json!({ "job_id": "job-recorded", "message_type": "success" }),
            )
            .await?;

        let start = S3_DECODE.checkpoint();
        let (_, resolved, failures) =
            settle_expired_leases(&*todo, &*submissions, &HashMap::new(), 3, now).await?;

        assert_eq!(failures, 0);
        assert_eq!(resolved, HashSet::from([recorded]));
        assert_eq!(S3_DECODE.checkpoint().since(start).bodies, 0);
        Ok(())
    }

    /// A re-stamp can move a larger run's deadline between the submit route's
    /// read and the delete; the failed job still leaves the queue.
    #[tokio::test]
    async fn a_larger_run_whose_deadline_moved_after_the_read_leaves_the_queue()
    -> anyhow::Result<()> {
        let Some((todo, _db)) = sql_todo().await? else {
            return Ok(());
        };
        let submissions = pipette_store_s3::in_memory_submission_store();
        let benchmark = BenchmarkId::try_new("bench-1")?;
        let catalog = HashMap::from([(
            benchmark.clone(),
            Benchmark::from_toml(
                "bench-1",
                "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 100",
            )?,
        )]);
        let body = pipette_store_testkit::todo_job_body("job-larger", &["client1"], "bench-1");
        let job = pipette_store_testkit::seed_avail(&*todo, "job-larger", ExpiresAt::Never, &body)
            .await?;
        let read = AvailEntry {
            job_id: job.clone(),
            expires_at: ExpiresAt::Never,
        };
        assert_eq!(
            todo.redate_avail(&job, ExpiresAt::Never, "20980101T000000Z".parse()?)
                .await?,
            RedateResult::Redated
        );

        let failed = fail_larger_run(
            &*todo,
            &*submissions,
            &catalog,
            &read,
            &body,
            &JobId::try_new("job-smaller")?,
            &benchmark,
            FailureCode::AttemptTimeout,
            Utc::now(),
        )
        .await?;

        assert!(failed);
        assert!(
            todo.avail_for_jobs(std::slice::from_ref(&job))
                .await?
                .is_empty()
        );
        Ok(())
    }

    fn settle_test_client(id: &str, pk_suffix: &str, chip: &str) -> anyhow::Result<Client> {
        // 64-char hex pubkey with a distinct 2-char suffix so clients differ.
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: pipette_plan_types::PublicKeyHex::try_new(format!(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567{pk_suffix}"
            ))?,
            organization: pipette_plan_types::NonEmptyTrimmedString::try_new("org")?,
            client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("details")?,
            contact_email: pipette_plan_types::ContactEmail::try_new("a@b.com")?,
            status: pipette_core::client::ClientStatus::Approved,
            registered_at: Utc::now(),
            device_profile: pipette_plan_types::DeviceProfile {
                device_chip_model: Some(pipette_plan_types::NonEmptyTrimmedString::try_new(chip)?),
                ..Default::default()
            },
            capabilities: Default::default(),
        })
    }

    /// The pass replaces a flagged job's rows with the workers that match it,
    /// and clears the flag. A client outside the workers loses its row even
    /// though it matches; a job the run did not list keeps its flag.
    #[tokio::test]
    async fn index_rebuilds_each_listed_flagged_job() -> anyhow::Result<()> {
        use pipette_store_testkit::seed_avail;
        let Some((todo, _db)) = sql_todo().await? else {
            return Ok(());
        };
        let mismatched = settle_test_client("index-mismatch", "00", "chip-b")?;
        let matched = settle_test_client("index-match", "11", "chip-a")?;
        let outsider = settle_test_client("index-outsider", "99", "chip-a")?;
        let body = serde_json::json!({"job_id": "job-index", "requires": ["chip:chip-a"]});
        let job = seed_avail(&*todo, "job-index", ExpiresAt::Never, &body).await?;
        let unlisted = seed_avail(&*todo, "job-unlisted", ExpiresAt::Never, &body).await?;
        todo.write_eligible(&mismatched.client_id, &job, ExpiresAt::Never)
            .await?;
        todo.write_eligible(&outsider.client_id, &job, ExpiresAt::Never)
            .await?;

        let flagged = todo
            .jobs_needing_reindex(std::slice::from_ref(&job))
            .await?
            .jobs;
        let indexed = index_flagged_jobs(
            &*todo,
            &[&mismatched, &matched],
            &FleetTags::new(Vec::new()),
            &flagged,
        )
        .await?;

        assert_eq!(
            todo.list_all_eligible().await?,
            vec![(matched.client_id.clone(), job.clone(), ExpiresAt::Never)]
        );
        let still_flagged: Vec<JobId> = todo
            .jobs_needing_reindex(std::slice::from_ref(&unlisted))
            .await?
            .jobs
            .into_iter()
            .map(|flagged| flagged.job_id)
            .collect();
        assert_eq!(still_flagged, vec![unlisted]);
        assert_eq!(indexed, HashSet::from([job]));
        Ok(())
    }

    struct Starvation {
        _dir: tempfile::TempDir,
        _db: TestDb,
        todo: std::sync::Arc<dyn TodoStore>,
        plans: std::sync::Arc<dyn PlanStore>,
        submissions: std::sync::Arc<dyn SubmissionStore>,
        client: Client,
        job: JobId,
        flagged: Vec<ReindexJob>,
        tags: FleetTags,
        manifest: PlanManifest,
    }

    impl Starvation {
        async fn new() -> anyhow::Result<Option<Self>> {
            let Some((todo, db)) = sql_todo().await? else {
                return Ok(None);
            };
            let dir = tempfile::tempdir()?;
            let plans = pipette_store_fs::build_local_fs_plan_store(&StorageConfig::local_fs(
                dir.path().to_path_buf(),
            ))?;
            let job = pipette_store_testkit::seed_avail(
                &*todo,
                "job-starvation",
                ExpiresAt::Never,
                &serde_json::json!({"requires": ["chip:chip-a"]}),
            )
            .await?;
            let flagged = todo
                .jobs_needing_reindex(std::slice::from_ref(&job))
                .await?
                .jobs;
            let mut manifest = PlanManifest {
                campaign_id: None,
                ..pipette_core::plan::sample_manifest(
                    PlanId::try_new("plan-00000000-0000-0000-0000-000000000001")?,
                    PlanStatus::Active,
                    false,
                )?
            };
            manifest.job_ids = vec![job.clone()];
            Ok(Some(Self {
                _dir: dir,
                _db: db,
                todo,
                plans,
                submissions: pipette_store_s3::in_memory_submission_store(),
                client: settle_test_client("starvation-client", "11", "chip-a")?,
                job,
                flagged,
                tags: FleetTags::new(Vec::new()),
                manifest,
            }))
        }

        fn avail(&self) -> AvailEntry {
            AvailEntry {
                job_id: self.job.clone(),
                expires_at: ExpiresAt::Never,
            }
        }
    }

    #[rstest]
    #[case::withheld(false, false)]
    #[case::revision_race(true, false)]
    #[case::denied_fallback(true, true)]
    #[tokio::test]
    async fn starvation_matches_unindexed_jobs_from_the_cached_body(
        #[case] race: bool,
        #[case] denied: bool,
    ) -> anyhow::Result<()> {
        let Some(fixture) = Starvation::new().await? else {
            return Ok(());
        };
        let mut bodies = fixture
            .flagged
            .iter()
            .map(|row| (reindex_entry(row), row.body.clone()))
            .collect();
        let denier = if denied {
            fixture.client.client_id.clone()
        } else {
            ClientId::try_new("other-client")?
        };
        let denied_rows = if race {
            fixture
                .todo
                .write_denied(&fixture.job, &denier, &DenialRecord::default())
                .await?;
            vec![(fixture.job.clone(), denier)]
        } else {
            Vec::new()
        };
        let indexed = if race {
            index_flagged_jobs(
                &*fixture.todo,
                &[&fixture.client],
                &fixture.tags,
                &fixture.flagged,
            )
            .await?
        } else {
            HashSet::new()
        };
        assert!(indexed.is_empty());
        let fallback = HashSet::from([fixture.job.clone()]);
        let report = plan_maintenance::reconcile_plans(
            &*fixture.plans,
            &*fixture.todo,
            &*fixture.submissions,
            std::slice::from_ref(&fixture.manifest),
            &HashSet::new(),
            &plan_maintenance::PlanEndings {
                released: HashMap::new(),
                perturbed: HashSet::new(),
                stranded: HashSet::new(),
            },
            &plan_maintenance::Membership::new(Vec::new()),
            &[fixture.avail()],
            &HashSet::new(),
            &fallback,
            &[&fixture.client],
            &fixture.tags,
            &indexed,
            &fallback,
            &denied_rows,
            &mut bodies,
            Utc::now(),
            Duration::from_secs(60),
        )
        .await?;
        assert_eq!(report.failures, 0);
        let updated = fixture
            .plans
            .get_plan(&fixture.manifest.plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("updated plan"))?;
        assert_eq!(
            updated.status,
            if denied {
                PlanStatus::PendingClients
            } else {
                PlanStatus::Active
            }
        );
        let snapshot = updated
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?;
        assert_eq!(snapshot.starved.len(), usize::from(denied));
        assert_eq!(bodies.len(), 1);
        assert!(
            fixture
                .todo
                .jobs_awaiting_index(&[fixture.job])
                .await?
                .len()
                == 1
        );
        Ok(())
    }

    #[rstest]
    #[case::claimed_after_listing(true)]
    #[case::still_available(false)]
    #[tokio::test]
    async fn starvation_grouping_confirms_availability_before_plan_status(
        #[case] claimed: bool,
    ) -> anyhow::Result<()> {
        let Some(fixture) = Starvation::new().await? else {
            return Ok(());
        };
        index_flagged_jobs(
            &*fixture.todo,
            &[&fixture.client],
            &fixture.tags,
            &fixture.flagged,
        )
        .await?;
        assert!(
            fixture
                .todo
                .jobs_awaiting_index(std::slice::from_ref(&fixture.job))
                .await?
                .is_empty()
        );
        // The run listed an available job and no lease. A claim and a client
        // rebuild can then remove its eligibility before plan reconciliation.
        if claimed {
            assert!(matches!(
                fixture
                    .todo
                    .claim_job(
                        &fixture.job,
                        ExpiresAt::Never,
                        &fixture.client.client_id,
                        Utc::now() + chrono::Duration::hours(1)
                    )
                    .await?,
                pipette_store::ClaimResult::Claimed(_)
            ));
        }
        fixture
            .todo
            .delete_eligible_for_client(&fixture.client.client_id)
            .await?;
        let observed = pipette_store_testkit::ObservedTodoStore::new(fixture.todo.clone());
        let before = fixture.todo.queue_body_reads();
        let live = HashSet::from([fixture.job.clone()]);
        let mut bodies = HashMap::new();
        let report = plan_maintenance::reconcile_plans(
            &*fixture.plans,
            &observed,
            &*fixture.submissions,
            std::slice::from_ref(&fixture.manifest),
            &HashSet::new(),
            &plan_maintenance::PlanEndings {
                released: HashMap::new(),
                perturbed: HashSet::new(),
                stranded: HashSet::new(),
            },
            &plan_maintenance::Membership::new(Vec::new()),
            &[fixture.avail()],
            &HashSet::new(),
            &live,
            &[&fixture.client],
            &fixture.tags,
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &mut bodies,
            Utc::now(),
            Duration::from_secs(60),
        )
        .await?;
        assert_eq!(report.failures, 0);
        let updated = fixture
            .plans
            .get_plan(&fixture.manifest.plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("updated plan"))?;
        assert_eq!(
            updated.status,
            if claimed {
                PlanStatus::Active
            } else {
                PlanStatus::PendingClients
            }
        );
        let snapshot = updated
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?;
        if claimed {
            assert!(snapshot.starved.is_empty());
        } else {
            assert_eq!(snapshot.starved.len(), 1);
            assert_eq!(snapshot.starved[0].job_ids, vec![fixture.job.clone()]);
            assert_eq!(snapshot.starved[0].clauses.requires, vec!["chip:chip-a"]);
        }
        assert_eq!(
            observed.take_reads(),
            vec![("get_avail", vec![fixture.job])]
        );
        assert_eq!(
            fixture.todo.queue_body_reads() - before,
            u64::from(!claimed)
        );
        assert_eq!(bodies.len(), usize::from(!claimed));
        observed.unexpected.none()?;
        Ok(())
    }

    type ScriptedRead = Result<Option<Client>, String>;

    /// Test-only [`ClientStore`] that scripts successive `get_client` results.
    /// Any other call, or a read past the script, lands in `unexpected`.
    struct ScriptedClientStore {
        reads: std::sync::Mutex<std::collections::VecDeque<ScriptedRead>>,
        unexpected: pipette_store_testkit::UnexpectedCalls,
    }

    impl ScriptedClientStore {
        fn new(reads: impl IntoIterator<Item = ScriptedRead>) -> Self {
            Self {
                reads: std::sync::Mutex::new(reads.into_iter().collect()),
                unexpected: pipette_store_testkit::UnexpectedCalls::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ClientStore for ScriptedClientStore {
        async fn get_client(&self, _client_id: &ClientId) -> anyhow::Result<Option<Client>> {
            self.reads
                .lock()
                .map_err(|e| anyhow::anyhow!("scripted client store poisoned: {e}"))?
                .pop_front()
                .ok_or_else(|| self.unexpected.record("get_client"))?
                .map_err(|e| anyhow::anyhow!(e))
        }
        async fn put_client(&self, _client: &Client) -> anyhow::Result<()> {
            Err(self.unexpected.record("put_client"))
        }
        async fn delete_client(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_client"))
        }
        async fn list_clients(
            &self,
            _after: Option<&ClientId>,
            _limit: std::num::NonZeroUsize,
        ) -> anyhow::Result<pipette_store::ClientPage> {
            Err(self.unexpected.record("list_clients"))
        }
        async fn list_client_ids(&self) -> anyhow::Result<Vec<ClientId>> {
            Err(self.unexpected.record("list_client_ids"))
        }
        async fn has_public_key(
            &self,
            _public_key: &pipette_plan_types::PublicKeyHex,
        ) -> anyhow::Result<bool> {
            Err(self.unexpected.record("has_public_key"))
        }
        async fn get_client_permissions(
            &self,
            _client_id: &ClientId,
        ) -> anyhow::Result<Option<ClientPermissionSet>> {
            Err(self.unexpected.record("get_client_permissions"))
        }
        async fn list_client_permissions(
            &self,
        ) -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>> {
            Err(self.unexpected.record("list_client_permissions"))
        }
        async fn put_client_permissions(
            &self,
            _client_id: &ClientId,
            _set: &ClientPermissionSet,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("put_client_permissions"))
        }
        async fn delete_client_permissions(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_client_permissions"))
        }
        async fn mutate_client_permissions(
            &self,
            _client_id: &ClientId,
            _f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
        ) -> anyhow::Result<pipette_store::ClientPermissionMutation> {
            Err(self.unexpected.record("mutate_client_permissions"))
        }
        async fn has_signature_migration(&self, _client_id: &ClientId) -> anyhow::Result<bool> {
            Err(self.unexpected.record("has_signature_migration"))
        }
        async fn record_signature_migration(
            &self,
            _client_id: &ClientId,
            _at: DateTime<Utc>,
        ) -> anyhow::Result<pipette_core::client::MigrationRecord> {
            Err(self.unexpected.record("record_signature_migration"))
        }
        async fn list_signature_migrations(
            &self,
        ) -> anyhow::Result<Vec<(ClientId, pipette_core::client::SignatureMigration)>> {
            Err(self.unexpected.record("list_signature_migrations"))
        }
        async fn put_client_preauth_key(
            &self,
            _key: &pipette_core::client_preauth::ClientPreauthKey,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("put_client_preauth_key"))
        }
        async fn get_client_preauth_key(
            &self,
            _key_id: &pipette_plan_types::ClientPreauthKeyId,
        ) -> anyhow::Result<Option<pipette_core::client_preauth::ClientPreauthKey>> {
            Err(self.unexpected.record("get_client_preauth_key"))
        }
        async fn consume_client_preauth_key(
            &self,
            _key_id: &pipette_plan_types::ClientPreauthKeyId,
            _secret: &pipette_core::client_preauth::ClientPreauthSecret,
        ) -> anyhow::Result<pipette_core::client_preauth::ClientPreauthConsumeOutcome> {
            Err(self.unexpected.record("consume_client_preauth_key"))
        }
        async fn list_client_preauth_keys(
            &self,
        ) -> anyhow::Result<Vec<pipette_core::client_preauth::ClientPreauthKey>> {
            Err(self.unexpected.record("list_client_preauth_keys"))
        }
        async fn delete_client_preauth_key(
            &self,
            _key_id: &pipette_plan_types::ClientPreauthKeyId,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_client_preauth_key"))
        }
        async fn list_spent_client_preauth_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_plan_types::ClientPreauthKeyId>> {
            Err(self.unexpected.record("list_spent_client_preauth_markers"))
        }
        async fn delete_spent_client_preauth_marker(
            &self,
            _key_id: &pipette_plan_types::ClientPreauthKeyId,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_spent_client_preauth_marker"))
        }
    }

    /// One `reindex_flagged_clients` call over a database todo store; the
    /// inputs a test does not set start empty.
    struct Reindex<T = LocalFsTagStore> {
        _dir: tempfile::TempDir,
        _db: TestDb,
        todo: std::sync::Arc<dyn TodoStore>,
        tags: T,
        workers: HashSet<ClientId>,
        joined: BTreeSet<ClientId>,
        keys: Vec<AvailEntry>,
        leased: HashSet<JobId>,
        bodies: HashMap<AvailEntry, Value>,
    }

    impl Reindex {
        /// `None` skips the test without `TEST_DATABASE_URL`.
        async fn new() -> anyhow::Result<Option<Self>> {
            let dir = tempfile::tempdir()?;
            let tags = LocalFsTagStore::new(dir.path().to_path_buf());
            Self::in_dir(dir, tags).await
        }
    }

    impl<T: TagStore> Reindex<T> {
        async fn with_tags(tags: T) -> anyhow::Result<Option<Self>> {
            Self::in_dir(tempfile::tempdir()?, tags).await
        }

        async fn in_dir(dir: tempfile::TempDir, tags: T) -> anyhow::Result<Option<Self>> {
            let Some((todo, db)) = sql_todo().await? else {
                return Ok(None);
            };
            Ok(Some(Self {
                _dir: dir,
                _db: db,
                todo,
                tags,
                workers: HashSet::new(),
                joined: BTreeSet::new(),
                keys: Vec::new(),
                leased: HashSet::new(),
                bodies: HashMap::new(),
            }))
        }

        async fn run(&self, auth: &dyn ClientStore) -> anyhow::Result<HashSet<ClientId>> {
            stream::iter(
                self.keys
                    .iter()
                    .filter_map(|key| self.bodies.get(key).map(|body| (key, body))),
            )
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|(key, body)| async move {
                pipette_store_testkit::seed_avail(
                    &*self.todo,
                    key.job_id.as_str(),
                    key.expires_at,
                    body,
                )
                .await
                .map(|_| ())
            })
            .await?;
            reindex_flagged_clients(
                &*self.todo,
                auth,
                &self.tags,
                &self.workers,
                &self.joined,
                &self
                    .keys
                    .iter()
                    .map(|key| key.job_id.clone())
                    .collect::<Vec<_>>(),
                &self.leased,
            )
            .await
        }
    }

    #[tokio::test]
    async fn client_rebuilds_share_one_clause_group_read() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let a = settle_test_client("batch-a", "11", "chip-a")?;
        let b = settle_test_client("batch-b", "22", "chip-b")?;
        let auth = ScriptedClientStore::new([
            Ok(Some(a.clone())),
            Ok(Some(a.clone())),
            Ok(Some(b.clone())),
            Ok(Some(b.clone())),
        ]);
        let job_a = JobId::try_new("job-batch-a")?;
        let job_b = JobId::try_new("job-batch-b")?;
        let key_a = AvailEntry {
            job_id: job_a.clone(),
            expires_at: ExpiresAt::Never,
        };
        let key_b = AvailEntry {
            job_id: job_b.clone(),
            expires_at: ExpiresAt::Never,
        };
        r.keys = vec![key_a.clone(), key_b.clone()];
        r.bodies = HashMap::from([
            (key_a, serde_json::json!({"requires": ["chip:chip-a"]})),
            (key_b, serde_json::json!({"requires": ["chip:chip-b"]})),
        ]);
        r.workers = HashSet::from([a.client_id.clone(), b.client_id.clone()]);
        r.joined = BTreeSet::from([a.client_id.clone(), b.client_id.clone()]);
        let observed = std::sync::Arc::new(pipette_store_testkit::ObservedTodoStore::new(
            r.todo.clone(),
        ));
        r.todo = observed.clone();
        r.run(&auth).await?;
        assert_eq!(
            observed.take_reads(),
            vec![("avail_clause_groups", vec![job_a.clone(), job_b.clone()])]
        );
        assert_eq!(
            r.todo.list_all_eligible().await?,
            vec![
                (a.client_id, job_a, ExpiresAt::Never),
                (b.client_id, job_b, ExpiresAt::Never),
            ]
        );
        assert_eq!(r.todo.queue_body_reads(), 0);
        auth.unexpected.none()?;
        observed.unexpected.none()?;
        Ok(())
    }

    /// A record that still matches at the recheck consumes the flags as usual.
    #[rstest]
    #[case::record_changed(true)]
    #[case::record_unchanged(false)]
    #[tokio::test]
    async fn reindex_keeps_captured_flags_when_record_changes_mid_rebuild(
        #[case] changed: bool,
    ) -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };

        // The rebuild reads the first record; the recheck reads the second.
        let evaluated = settle_test_client("recheck-client", "22", "chip-a")?;
        let at_recheck = if changed {
            settle_test_client("recheck-client", "22", "chip-b")?
        } else {
            evaluated.clone()
        };
        let auth = ScriptedClientStore::new([Ok(Some(evaluated.clone())), Ok(Some(at_recheck))]);

        r.todo.write_pending_reindex(&evaluated.client_id).await?;

        let job = JobId::new_unchecked("job-recheck");
        let avail = AvailEntry {
            job_id: job.clone(),
            expires_at: ExpiresAt::Never,
        };
        r.bodies.insert(
            avail.clone(),
            serde_json::json!({
                "job_id": "job-recheck",
                "requires": ["chip:chip-a"],
            }),
        );
        r.keys.push(avail);
        r.workers.insert(evaluated.client_id.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;

        // Either way the rebuild ran against the record it read first.
        assert_eq!(
            r.todo.list_all_eligible().await?,
            vec![(evaluated.client_id.clone(), job, ExpiresAt::Never)]
        );
        assert_eq!(
            r.todo.list_pending_reindex().await?.is_empty(),
            !changed,
            "a record that changed under the rebuild must keep its captured flags"
        );
        Ok(())
    }

    /// The run also continues past the failed read.
    #[tokio::test]
    async fn reindex_keeps_flags_when_the_recheck_read_fails() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };

        let evaluated = settle_test_client("recheck-io-fail", "33", "chip-a")?;
        let auth = ScriptedClientStore::new([
            Ok(Some(evaluated.clone())),
            Err("auth store unavailable".into()),
        ]);

        r.todo.write_pending_reindex(&evaluated.client_id).await?;

        r.workers.insert(evaluated.client_id.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;

        assert_eq!(
            r.todo.list_pending_reindex().await?.len(),
            1,
            "a recheck read failure must keep the captured flags"
        );
        Ok(())
    }

    /// Test-only [`TagStore`] that scripts successive `get_client_tags`
    /// results. Any other call, or a read past the script, lands in
    /// `unexpected`.
    struct ScriptedTagStore {
        reads: std::sync::Mutex<
            std::collections::VecDeque<std::collections::BTreeSet<pipette_plan_types::Tag>>,
        >,
        unexpected: pipette_store_testkit::UnexpectedCalls,
    }

    impl ScriptedTagStore {
        fn new(
            reads: impl IntoIterator<Item = std::collections::BTreeSet<pipette_plan_types::Tag>>,
        ) -> Self {
            Self {
                reads: std::sync::Mutex::new(reads.into_iter().collect()),
                unexpected: pipette_store_testkit::UnexpectedCalls::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl TagStore for ScriptedTagStore {
        async fn get_client_tags(
            &self,
            _client_id: &ClientId,
        ) -> anyhow::Result<std::collections::BTreeSet<pipette_plan_types::Tag>> {
            self.reads
                .lock()
                .map_err(|e| anyhow::anyhow!("scripted tag store poisoned: {e}"))?
                .pop_front()
                .ok_or_else(|| self.unexpected.record("get_client_tags"))
        }
        async fn add_client_tag(
            &self,
            _client_id: &ClientId,
            _tag: &pipette_plan_types::Tag,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("add_client_tag"))
        }
        async fn remove_client_tag(
            &self,
            _client_id: &ClientId,
            _tag: &pipette_plan_types::Tag,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("remove_client_tag"))
        }
        async fn delete_client_tags(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_client_tags"))
        }
        async fn list_client_ids_by_tag(
            &self,
            _tag: &pipette_plan_types::Tag,
        ) -> anyhow::Result<Vec<ClientId>> {
            Err(self.unexpected.record("list_client_ids_by_tag"))
        }
        async fn list_forward_tag_markers(
            &self,
        ) -> anyhow::Result<Vec<(ClientId, pipette_plan_types::Tag)>> {
            Err(self.unexpected.record("list_forward_tag_markers"))
        }
        async fn list_reverse_tag_markers(
            &self,
        ) -> anyhow::Result<Vec<(ClientId, pipette_plan_types::Tag)>> {
            Err(self.unexpected.record("list_reverse_tag_markers"))
        }
    }

    #[tokio::test]
    async fn reindex_rebuild_matches_the_clients_tags() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let client = settle_test_client("tagged-client", "44", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        r.tags
            .add_client_tag(&client.client_id, &pipette_plan_types::Tag::try_new("lab")?)
            .await?;
        r.todo.write_pending_reindex(&client.client_id).await?;

        let tagged_job = JobId::new_unchecked("job-tagged");
        let tagged = AvailEntry {
            job_id: tagged_job.clone(),
            expires_at: ExpiresAt::Never,
        };
        let other = AvailEntry {
            job_id: JobId::new_unchecked("job-other-tag"),
            expires_at: ExpiresAt::Never,
        };
        r.keys = vec![tagged.clone(), other.clone()];
        r.bodies = HashMap::from([
            (
                tagged,
                serde_json::json!({"job_id": "job-tagged", "requires": ["tag:lab"]}),
            ),
            (
                other,
                serde_json::json!({"job_id": "job-other-tag", "requires": ["tag:other"]}),
            ),
        ]);

        r.workers.insert(client.client_id.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;

        assert_eq!(
            r.todo.list_all_eligible().await?,
            vec![(client.client_id.clone(), tagged_job, ExpiresAt::Never)]
        );
        assert!(r.todo.list_pending_reindex().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn reindex_flags_a_job_that_left_avail_after_the_listing() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let job = seed_indexed_lease(&*r.todo, "job-moved-during-rebuild").await?;
        let client = settle_test_client("rebuild-client", "11", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        r.todo.write_pending_reindex(&client.client_id).await?;
        // The availability listing saw it, but the lease listing did not.
        r.keys.push(AvailEntry {
            job_id: job.clone(),
            expires_at: ExpiresAt::Never,
        });
        r.workers.insert(client.client_id.clone());
        let observed = std::sync::Arc::new(pipette_store_testkit::ObservedTodoStore::new(
            r.todo.clone(),
        ));
        r.todo = observed.clone();
        r.run(&auth).await?;
        assert_eq!(
            observed.take_reads(),
            vec![("avail_clause_groups", vec![job.clone()])]
        );
        assert_eq!(
            r.todo
                .jobs_awaiting_index(std::slice::from_ref(&job))
                .await?,
            HashSet::from([job])
        );
        assert!(r.todo.list_all_eligible().await?.is_empty());
        auth.unexpected.none()?;
        observed.unexpected.none()?;
        Ok(())
    }

    /// No record read, and no leased job flagged: the cleanup covers them.
    #[tokio::test]
    async fn reindex_clears_a_flagged_client_that_is_not_a_worker() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let auth = ScriptedClientStore::new([]);
        let client = ClientId::try_new("ev1_pending")?;
        let job = seed_indexed_lease(&*r.todo, "job-held").await?;
        r.todo
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        r.todo.write_pending_reindex(&client).await?;

        r.leased.insert(job.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;

        assert!(r.todo.list_all_eligible().await?.is_empty());
        assert!(r.todo.list_pending_reindex().await?.is_empty());
        assert!(r.todo.jobs_awaiting_index(&[job]).await?.is_empty());
        Ok(())
    }

    /// The leased jobs are still flagged, because the client counted as a worker.
    #[tokio::test]
    async fn reindex_clears_a_flagged_worker_deleted_mid_run() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let auth = ScriptedClientStore::new([Ok(None)]);
        let client = ClientId::try_new("ev1_deleted")?;
        let job = pipette_store_testkit::seed_avail(
            &*r.todo,
            "job-held",
            ExpiresAt::Never,
            &serde_json::json!({"job_id": "job-held"}),
        )
        .await?;
        let leased = seed_indexed_lease(&*r.todo, "job-leased").await?;
        r.todo
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        r.todo.write_pending_reindex(&client).await?;

        r.workers.insert(client);
        r.leased.insert(leased.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;

        assert!(r.todo.list_all_eligible().await?.is_empty());
        assert!(r.todo.list_pending_reindex().await?.is_empty());
        assert_eq!(
            r.todo
                .jobs_awaiting_index(std::slice::from_ref(&leased))
                .await?,
            HashSet::from([leased])
        );
        Ok(())
    }

    /// A joiner gets no `pending-reindex/` flag; unchanged workers return none.
    #[tokio::test]
    async fn worker_changes_return_joiners_and_clear_leavers() -> anyhow::Result<()> {
        let Some((todo, _db)) = sql_todo().await? else {
            return Ok(());
        };
        let stays = ClientId::try_new("ev1_stays")?;
        let leaves = ClientId::try_new("ev1_leaves")?;
        let joins = ClientId::try_new("ev1_joins")?;
        let job = pipette_store_testkit::seed_avail(
            &*todo,
            "job-held",
            ExpiresAt::Never,
            &serde_json::json!({"job_id": "job-held"}),
        )
        .await?;
        let indexed = HashSet::from([stays.clone(), leaves.clone()]);
        todo.write_eligible(&stays, &job, ExpiresAt::Never).await?;
        todo.write_eligible(&leaves, &job, ExpiresAt::Never).await?;

        let workers = HashSet::from([stays.clone(), joins.clone()]);
        let joined = track_worker_changes(&*todo, &indexed, &workers).await?;

        assert_eq!(joined, BTreeSet::from([joins]));
        assert_eq!(
            todo.list_all_eligible().await?,
            vec![(stays, job, ExpiresAt::Never)]
        );
        assert!(todo.list_pending_reindex().await?.is_empty());
        assert!(
            track_worker_changes(&*todo, &workers, &workers)
                .await?
                .is_empty()
        );
        Ok(())
    }

    /// The leased jobs get the reindex flag, as for any rebuild.
    #[tokio::test]
    async fn reindex_rebuilds_a_joiner_without_a_flag() -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let client = settle_test_client("joined-client", "55", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        let stale = JobId::new_unchecked("job-stale");
        let leased = seed_indexed_lease(&*r.todo, "job-leased").await?;
        r.todo
            .write_eligible(&client.client_id, &stale, ExpiresAt::Never)
            .await?;

        r.workers.insert(client.client_id.clone());
        r.joined.insert(client.client_id.clone());
        r.leased.insert(leased.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;

        assert!(r.todo.list_all_eligible().await?.is_empty());
        assert!(r.todo.list_pending_reindex().await?.is_empty());
        assert_eq!(
            r.todo
                .jobs_awaiting_index(std::slice::from_ref(&leased))
                .await?,
            HashSet::from([leased])
        );
        Ok(())
    }

    #[derive(Clone, Copy, Debug)]
    enum JoinerRecheck {
        Unchanged,
        Changed,
        Unavailable,
        Deleted,
    }

    #[rstest]
    #[case::unchanged(JoinerRecheck::Unchanged, false)]
    #[case::changed(JoinerRecheck::Changed, true)]
    #[case::unavailable(JoinerRecheck::Unavailable, true)]
    #[case::deleted(JoinerRecheck::Deleted, true)]
    #[tokio::test]
    async fn an_unfinished_joiner_keeps_its_retry_trigger(
        #[case] recheck: JoinerRecheck,
        #[case] retry: bool,
    ) -> anyhow::Result<()> {
        let Some(mut r) = Reindex::new().await? else {
            return Ok(());
        };
        let client = settle_test_client("retry-joiner", "55", "chip-a")?;
        let fresh = match recheck {
            JoinerRecheck::Unchanged => Ok(Some(client.clone())),
            JoinerRecheck::Changed => Ok(Some(settle_test_client("retry-joiner", "55", "chip-b")?)),
            JoinerRecheck::Unavailable => Err("auth store unavailable".to_owned()),
            JoinerRecheck::Deleted => Ok(None),
        };
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), fresh]);
        r.workers.insert(client.client_id.clone());
        r.joined.insert(client.client_id.clone());

        let retry_joiners = r.run(&auth).await?;

        assert_eq!(retry_joiners.contains(&client.client_id), retry);
        assert!(r.todo.list_pending_reindex().await?.is_empty());
        auth.unexpected.none()?;
        Ok(())
    }

    #[tokio::test]
    async fn reindex_keeps_captured_flags_when_tags_change_mid_rebuild() -> anyhow::Result<()> {
        let client = settle_test_client("retagged-client", "55", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        // The rebuild reads no tag; the recheck reads the one written since.
        let Some(mut r) = Reindex::with_tags(ScriptedTagStore::new([
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::from([pipette_plan_types::Tag::try_new("lab")?]),
        ]))
        .await?
        else {
            return Ok(());
        };
        r.todo.write_pending_reindex(&client.client_id).await?;

        r.workers.insert(client.client_id.clone());
        r.run(&auth).await?;
        auth.unexpected.none()?;
        r.tags.unexpected.none()?;

        assert_eq!(
            r.todo.list_pending_reindex().await?.len(),
            1,
            "a tag change under the rebuild must keep the captured flags"
        );
        Ok(())
    }

    // ── claimable markers ──────────────────────────────────────────────────

    fn claim_now() -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(1_800_000_000)
    }

    fn deadline(offset: i64) -> ExpiresAt {
        ExpiresAt::At(claim_now() + chrono::Duration::seconds(offset))
    }

    /// The claimable markers of each client at [`claim_now`].
    fn backlog_of(
        eligible: &[(ClientId, JobId, ExpiresAt)],
        avail_expiries: &HashMap<JobId, ExpiresAt>,
        denied: &[(JobId, ClientId)],
    ) -> BTreeMap<ClientId, u64> {
        claimable_markers(eligible, avail_expiries, denied, claim_now())
            .into_iter()
            .fold(BTreeMap::new(), |mut acc, (client_id, _, _)| {
                *acc.entry(client_id.clone()).or_default() += 1;
                acc
            })
    }

    #[rstest]
    #[case::current_marker_counts("ev1_a", deadline(60), Some(deadline(60)), None, &[("ev1_a", 1)])]
    #[case::never_expiring_marker_counts("ev1_a", ExpiresAt::Never, Some(ExpiresAt::Never), None, &[("ev1_a", 1)])]
    #[case::leased_job("ev1_a", deadline(60), None, None, &[])]
    #[case::stale_dated_marker("ev1_a", deadline(60), Some(deadline(120)), None, &[])]
    #[case::deadline_at_now_counts("ev1_a", deadline(0), Some(deadline(0)), None, &[("ev1_a", 1)])]
    #[case::expired_job("ev1_a", deadline(-60), Some(deadline(-60)), None, &[])]
    #[case::denied_pair("ev1_a", deadline(60), Some(deadline(60)), Some("ev1_a"), &[])]
    #[case::another_clients_denial("ev1_a", deadline(60), Some(deadline(60)), Some("ev1_b"), &[("ev1_a", 1)])]
    #[case::another_clients_marker("ev1_b", deadline(60), Some(deadline(60)), None, &[("ev1_b", 1)])]
    fn claimable_markers_apply_the_claim_filters(
        #[case] holder: &str,
        #[case] marker: ExpiresAt,
        #[case] avail: Option<ExpiresAt>,
        #[case] denied_by: Option<&str>,
        #[case] expected: &[(&str, u64)],
    ) -> anyhow::Result<()> {
        let job = JobId::new_unchecked("job-1");
        let eligible = vec![(ClientId::try_new(holder)?, job.clone(), marker)];
        let avail_expiries: HashMap<JobId, ExpiresAt> =
            avail.map(|exp| (job.clone(), exp)).into_iter().collect();
        let denied = denied_by
            .map(|client| ClientId::try_new(client).map(|client| (job.clone(), client)))
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .map(|(client, jobs)| ClientId::try_new(*client).map(|client| (client, *jobs)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        assert_eq!(backlog_of(&eligible, &avail_expiries, &denied), expected);
        Ok(())
    }

    /// Seed one lease of `job` under `ExpiresAt::Never`. Set the job ID in its body.
    async fn seed_lease(
        todo: &dyn TodoStore,
        job: &JobId,
        client: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        seed_leased_until(
            todo,
            job.as_str(),
            ExpiresAt::Never,
            client,
            expiry,
            &serde_json::json!({ "job_id": job.as_str() }),
        )
        .await?;
        Ok(())
    }

    // ── run order ──────────────────────────────────────────────────────────

    /// A [`PlanStore`] whose first manifest listing lets a staging ingest finish
    /// first: its jobs promoted into `avail/` and its manifest written `active`.
    struct IngestFinishesAtManifestListing {
        inner: std::sync::Arc<dyn PlanStore>,
        todo: std::sync::Arc<dyn TodoStore>,
        finished: std::sync::atomic::AtomicBool,
        manifest: PlanManifest,
    }

    #[async_trait::async_trait]
    impl PlanStore for IngestFinishesAtManifestListing {
        async fn join_job(
            &self,
            job_id: &pipette_plan_types::JobId,
            plan_id: &pipette_plan_types::PlanId,
            expires_at: pipette_plan_types::ExpiresAt,
        ) -> anyhow::Result<pipette_store::JobJoin> {
            self.inner.join_job(job_id, plan_id, expires_at).await
        }

        async fn list_matching_plans(
            &self,
            filter: &pipette_store::PlanFilter,
            after: Option<&PlanId>,
            limit: std::num::NonZeroUsize,
        ) -> anyhow::Result<pipette_store::PlanPage> {
            if !self
                .finished
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                futures::future::try_join_all(
                    self.manifest
                        .job_ids
                        .iter()
                        .map(|job_id| self.todo.promote_avail(job_id, ExpiresAt::Never, None)),
                )
                .await?;
                self.inner.put_plan(&self.manifest).await?;
            }
            self.inner.list_matching_plans(filter, after, limit).await
        }
        async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
            self.inner.put_plan(manifest).await
        }
        async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
            self.inner.get_plan(plan_id).await
        }
        async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.inner.write_cancel_marker(plan_id).await
        }
        async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
            self.inner.has_cancel_marker(plan_id).await
        }
        async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
            self.inner.list_cancel_markers().await
        }
        async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.inner.delete_cancel_marker(plan_id).await
        }
        async fn get_content_key(
            &self,
            digest: &str,
        ) -> anyhow::Result<
            Option<(
                pipette_store::ContentKeyRecord,
                pipette_store::ContentKeyVersion,
            )>,
        > {
            self.inner.get_content_key(digest).await
        }
        async fn create_content_key(
            &self,
            digest: &str,
            record: &pipette_store::ContentKeyRecord,
        ) -> anyhow::Result<pipette_store::ContentKeyClaim> {
            self.inner.create_content_key(digest, record).await
        }
        async fn replace_content_key(
            &self,
            digest: &str,
            version: &pipette_store::ContentKeyVersion,
            record: &pipette_store::ContentKeyRecord,
        ) -> anyhow::Result<pipette_store::ContentKeyCas> {
            self.inner
                .replace_content_key(digest, version, record)
                .await
        }
        async fn delete_content_key(
            &self,
            digest: &str,
            version: &pipette_store::ContentKeyVersion,
        ) -> anyhow::Result<pipette_store::ContentKeyCas> {
            self.inner.delete_content_key(digest, version).await
        }
        async fn list_content_keys(
            &self,
        ) -> anyhow::Result<
            Vec<(
                String,
                pipette_store::ContentKeyRecord,
                pipette_store::ContentKeyVersion,
            )>,
        > {
            self.inner.list_content_keys().await
        }
        async fn write_job_plan_marker(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
            expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            self.inner
                .write_job_plan_marker(job_id, plan_id, expires_at)
                .await
        }
        async fn list_job_plan_markers(
            &self,
            job_id: &JobId,
        ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>> {
            self.inner.list_job_plan_markers(job_id).await
        }
        async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
            self.inner.list_all_job_plan_markers().await
        }
        async fn delete_job_plan_marker(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
            expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            self.inner
                .delete_job_plan_marker(job_id, plan_id, expires_at)
                .await
        }
        async fn end_membership(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
        ) -> anyhow::Result<pipette_store::MembershipEnd> {
            self.inner.end_membership(job_id, plan_id).await
        }
        async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
            self.inner.read_gc_candidates().await
        }
        async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
            self.inner.write_gc_candidates(candidates).await
        }
        async fn put_plan_source(
            &self,
            plan_id: &PlanId,
            source: &pipette_core::plan::PlanSource,
        ) -> anyhow::Result<CreateOutcome> {
            self.inner.put_plan_source(plan_id, source).await
        }
        async fn get_plan_source(
            &self,
            plan_id: &PlanId,
        ) -> anyhow::Result<Option<pipette_core::plan::PlanSource>> {
            self.inner.get_plan_source(plan_id).await
        }
        async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.inner.delete_plan_source(plan_id).await
        }
        async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.inner.write_pending_source_marker(plan_id).await
        }
        async fn list_pending_source_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_store::PendingSourceMarker>> {
            self.inner.list_pending_source_markers().await
        }
        async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.inner.delete_pending_source_marker(plan_id).await
        }
        async fn put_job_source(
            &self,
            job_id: &JobId,
            body: &Value,
        ) -> anyhow::Result<CreateOutcome> {
            self.inner.put_job_source(job_id, body).await
        }
        async fn get_job_source(&self, job_id: &JobId) -> anyhow::Result<Option<Value>> {
            self.inner.get_job_source(job_id).await
        }
        async fn put_job_plan_link(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
        ) -> anyhow::Result<CreateOutcome> {
            self.inner.put_job_plan_link(job_id, plan_id).await
        }
        async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
            self.inner.list_job_plan_links(job_id).await
        }
    }

    /// An ingest that finishes as the run lists the manifests leaves its plan
    /// live: the run counts the new job outstanding, not gone, so the plan does
    /// not latch `complete`.
    #[tokio::test]
    async fn a_plan_ingested_mid_run_is_not_latched_complete() -> anyhow::Result<()> {
        let Some(db) = TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let stores = db.stores(pipette_store_fs::build_local_fs_stores(
            &pipette_core::config::Config {
                storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                ..pipette_core::config::Config::default()
            },
            pipette_store_testkit::no_queue(),
        )?);
        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(1));
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        let creating = PlanManifest {
            plan_id: plan_id.clone(),
            status: PlanStatus::Creating,
            created_at: Utc::now(),
            submitted_by: None,
            campaign_id: None,
            previous_plan_id: None,
            source_sha256: None,
            submitted_sha256: None,
            plan_warnings: Vec::new(),
            benchmark_types: Default::default(),
            job_ids: vec![job_id.clone()],
            content_keys: Default::default(),
            rerun_of: Default::default(),
            warnings: Vec::new(),
            expired_jobs: BTreeMap::new(),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: None,
            terminal_at: None,
        };
        // The ingest's state before its promote.
        stores.plans.put_plan(&creating).await?;
        stores
            .plans
            .write_job_plan_marker(&job_id, &plan_id, ExpiresAt::Never)
            .await?;
        let body = serde_json::json!({"job_id": job_id.as_str(), "requires": ["os:macos"]});
        stores.todo.write_tmp(&job_id, &body).await?;
        let plans = IngestFinishesAtManifestListing {
            inner: stores.plans.clone(),
            todo: stores.todo.clone(),
            finished: std::sync::atomic::AtomicBool::new(false),
            manifest: PlanManifest {
                status: PlanStatus::Active,
                ..creating
            },
        };

        run(
            &*stores.todo,
            &*stores.clients,
            &*stores.tags,
            &*stores.submissions,
            &plans,
            &HashMap::new(),
            MaintenanceSettings {
                tmp_max: Duration::from_secs(86_400),
                plan_creating_max: Duration::from_secs(21_600),
                lost_lease_limit: 3,
            },
        )
        .await?;

        let manifest = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the plan has no manifest"))?;
        // No client is registered, so an outstanding job reads `pending_clients`.
        assert_eq!(manifest.status, PlanStatus::PendingClients);
        assert_eq!(manifest.terminal_at, None);
        let counts = manifest
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("the run wrote no snapshot"))?
            .counts;
        assert_eq!((counts.available, counts.cancelled), (1, 0));
        assert!(
            stores.todo.get_avail_by_job(&job_id).await?.is_some(),
            "the job stays claimable"
        );
        Ok(())
    }
}
