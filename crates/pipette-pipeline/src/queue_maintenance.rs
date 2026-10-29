//! The `pipette-mgmt queue-maintenance` command: all cron reconciliation for
//! the `todo/` job queue. Runs must be serialized and the host clock must not
//! run ahead of `serve`'s (`docs/operations.md`); see `docs/queue.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream;
use futures::stream::StreamExt;
use serde_json::Value;

use pipette_core::benchmark::Benchmark;
use pipette_core::client::Client;
use pipette_core::denial::{DeniedClient, summarize_denials};
use pipette_core::matching;
use pipette_core::matching::{FleetTags, capability_clauses_malformed, job_matches_client};
use pipette_core::plan::PlanManifest;
use pipette_core::todo_filename::{
    Lease, avail_filename, denied_marker_filename, eligible_filename, parse_avail_filename,
    parse_leased_key,
};
use pipette_plan_types::{
    BenchmarkId, CampaignId, ClientId, ExpiresAt, FailureSource, JobId, PlanId, PlanStatus,
};
use pipette_store::decode_stats::{DecodeCounts, S3_DECODE};
use pipette_store::http_stats::{HttpCounts, S3_HTTP};
use pipette_store::{
    BacklogSnapshot, CampaignBacklog, CampaignBacklogSnapshot, ClientStore, CreateOutcome,
    JobPlanMarker, PlanStore, RecycleResult, RedateResult, STORAGE_CONCURRENCY, SubmissionStore,
    TagStore, TodoStore, drain_clients, drain_plans, warn_unreadable_clients,
};

use crate::submission::{UnrecordableJobError, record_system_failure};
use crate::{plan_ingestion, plan_maintenance};

/// One `avail/` entry, parsed once in [`collect_all_avail`].
pub struct AvailKey {
    pub key: String,
    pub job_id: JobId,
    pub expires_at: ExpiresAt,
}

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
    let at_start = PhaseMark::now();
    let now = Utc::now();
    // Cleared at once, so a failed run leaves no sightings a later run would
    // mistake as consecutive; GC is then delayed one interval, the safe side.
    let prior_gc_candidates = todo.read_gc_candidates().await?;
    todo.write_gc_candidates(&HashSet::new()).await?;

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
    let avail_keys = collect_all_avail(todo).await?.keys;
    // Unparseable keys were already warned about in `settle_expired_leases`.
    let leases: Vec<Lease> = todo
        .list_leased()
        .await?
        .iter()
        .filter_map(|k| parse_leased_key(k).ok())
        .collect();

    // Teardown precedes every queue reader, so no pass indexes a doomed job.
    // One `job-plans/` listing per run, so a plan with many jobs costs none.
    let job_plan_markers = plans.list_all_job_plan_markers().await?;
    let membership = plan_maintenance::Membership::new(job_plan_markers.clone());
    let teardown = plan_maintenance::tear_down_plans(
        todo,
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
    // The `avail/` keys keep a recycled lease from being re-indexed as new.
    let leased_ids: HashSet<JobId> = teardown.leased.iter().map(|l| l.job_id.clone()).collect();
    let leased_avail_keys: HashSet<String> = teardown
        .leased
        .iter()
        .map(|l| avail_filename(&l.job_id, l.expires_at))
        .collect();

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

    let mut bodies: HashMap<String, Value> = HashMap::new();
    // Retirement first, so an unmatchable body is not reported as an exhausted
    // roster.
    let (all_keys, retired_now, retirement_failures) =
        retire_unmatchable_jobs(todo, submissions, catalog, all_keys, &mut bodies, now).await?;
    let (all_keys, escalated_now, escalation_failures) =
        escalate_all_denied_jobs(todo, submissions, catalog, all_keys, &mut bodies, now).await?;
    let failed_items = failed_items + expiry_failures + retirement_failures + escalation_failures;

    // Confirmed terminal for this run's GC sweeps.
    let mut terminal_now = expired_now;
    terminal_now.extend(retired_now);
    terminal_now.extend(escalated_now);
    terminal_now.extend(resolved_at_recycle);
    terminal_now.extend(teardown.retired);

    let clients = drain_clients(auth).await?;
    warn_unreadable_clients(&clients.unreadable);
    let mut client_records_drained = clients.items.len() + clients.unreadable.len();
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
    // One snapshot for every index pass and plan reconciliation, so the index
    // agrees with the persisted worker set; a mid-run change is next run's diff.
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

    let indexed_workers = todo.read_eligible_workers().await?;
    let joined = track_worker_changes(todo, &indexed_workers, &worker_ids).await?;
    index_new_jobs(
        todo,
        &workers,
        &fleet_tags,
        &all_keys,
        &leased_avail_keys,
        &mut bodies,
    )
    .await?;
    reindex_flagged_clients(
        todo,
        auth,
        tags,
        &worker_ids,
        &joined,
        &all_keys,
        &leased_ids,
        &mut bodies,
    )
    .await?;
    // After every joiner's rebuild, so a failed run sees the same joiners again.
    if indexed_workers != worker_ids {
        todo.write_eligible_workers(&worker_ids).await?;
    }
    client_records_drained += settle_pending_reindex_jobs(
        todo,
        auth,
        tags,
        submissions,
        &worker_ids,
        &all_keys,
        &mut bodies,
    )
    .await?;

    // After the index passes, so the sweep never deletes a marker they wrote.
    let live_clients: HashSet<ClientId> = clients.iter().map(|c| c.client_id.clone()).collect();
    // Tells a re-date's leftover `eligible/` marker from a current one. A job
    // with two `avail/` entries is omitted: none of its markers is provably stale.
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
    let eligible = todo.list_all_eligible().await?;
    let denied = todo.list_all_denied().await?;
    let mut next_candidates = reconcile_orphans(
        todo,
        submissions,
        &eligible,
        &denied,
        &live_ids,
        &live_clients,
        &avail_expiries,
        &terminal_now,
        &prior_gc_candidates,
    )
    .await?;
    // Written even when an item failed, so a persistent failure cannot freeze
    // the count. Console-only, so a failed write is just a warning.
    let claimable = claimable_markers(&eligible, &avail_expiries, &denied, now);
    let backlog = BacklogSnapshot {
        counted_at: now,
        jobs: count_backlog(&claimable),
    };
    match todo.write_backlog(&backlog).await {
        Ok(()) => tracing::info!(
            clients = backlog.jobs.len(),
            "queue-maintenance: backlog snapshot written"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "queue-maintenance: backlog snapshot write failed"
        ),
    }
    // Its own object, so the fleet listing reads only the totals.
    let campaign_backlog = CampaignBacklogSnapshot {
        counted_at: now,
        devices: count_campaign_backlog(
            &claimable,
            &job_campaigns(
                &job_plan_markers,
                &manifests,
                &cancel_markers,
                &teardown.endings.stranded,
                now,
            ),
        ),
    };
    match todo.write_campaign_backlog(&campaign_backlog).await {
        Ok(()) => tracing::info!(
            clients = campaign_backlog.devices.len(),
            "queue-maintenance: campaign backlog snapshot written"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "queue-maintenance: campaign backlog snapshot write failed"
        ),
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
    next_candidates.extend(index_gc.candidates);
    next_candidates.extend(source_candidates);
    todo.write_gc_candidates(&next_candidates).await?;

    let failed_items = failed_items + reconciled.failures + index_gc.failures + source_gc_failures;

    cleanup_stale_tmp(todo, settings.tmp_max).await?;

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
    // The candidate-set write and `tmp/` cleanup are in no phase, so the
    // phases sum to less than `duration_ms`.
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
        cpu_user_ms = run_cpu.map(|cpu| millis(cpu.user)),
        cpu_system_ms = run_cpu.map(|cpu| millis(cpu.system)),
        failed_items,
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
    for key in todo.list_leased().await? {
        // Foreign cruft or an older key format: left in place, not live, not a
        // failure. A key format change therefore needs an empty `leased/` tree.
        let Ok(lease) = parse_leased_key(&key) else {
            tracing::warn!(key = %key, "skipping unparseable leased key");
            continue;
        };
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
enum ExpiredLease {
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
async fn settle_expired_lease(
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
                std::future::ready(lost_lease_reason(&losses)),
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
/// re-date it onto the latest horizon a member plan still holds, or retire it.
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
    all_keys: Vec<AvailKey>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailKey>, HashSet<JobId>, usize)> {
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
            // Origin ambiguous, so not confirmed terminal; two-sighting GC owns it.
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
    /// Carries the entry's new key.
    Redated(AvailKey),
    Retired,
    Vanished,
}

/// Settle one job whose encoded deadline has lapsed: re-date it onto the latest
/// horizon its member plans still hold, or retire it when none remains.
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
    avail: &AvailKey,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<Overdue> {
    // Fresh, since the lapse pass just deleted markers. An error must propagate:
    // read as "no member", it would retire a job another plan still waits on.
    let latest = plans
        .list_job_plan_markers(&avail.job_id)
        .await?
        .into_iter()
        .map(|(_, expires_at)| expires_at)
        .filter(|horizon| !horizon.is_expired(now))
        .max();
    let Some(to) = latest else {
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
            // Its markers name the old deadline; the flag is the backstop for a
            // run that dies before the index passes rewrite them.
            todo.write_pending_reindex_job(&avail.job_id).await?;
            tracing::info!(
                job_id = %avail.job_id,
                from = %avail.expires_at,
                to = %to,
                "queue-maintenance: re-dated a job to the latest horizon its member plans hold"
            );
            Ok(Overdue::Redated(AvailKey {
                key: avail_filename(&avail.job_id, to),
                job_id: avail.job_id.clone(),
                expires_at: to,
            }))
        }
        // Claimed first; the lease recycles under the old key for a later run.
        RedateResult::Gone => Ok(Overdue::Vanished),
    }
}

/// Retire every `avail/` job whose body no possible client can match, as a
/// synthetic `"system"` failure; ingestion refuses one, so it came from a
/// bypassing writer or an older build.
///
/// Not part of [`escalate_all_denied_jobs`]: such a job collects no denials. A
/// job an older build indexed can be claimed while it is retired, leaving a
/// failure and a live lease, until `flag-reindex --all` runs.
async fn retire_unmatchable_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailKey>,
    bodies: &mut HashMap<String, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailKey>, HashSet<JobId>, usize)> {
    let mut survivors = Vec::with_capacity(all_keys.len());
    let mut removed = HashSet::new();
    let mut failures = 0usize;
    for avail in all_keys {
        match retire_one_if_unmatchable(todo, submissions, catalog, &avail, bodies, now).await {
            Ok(true) => {
                removed.insert(avail.job_id.clone());
            }
            Ok(false) => survivors.push(avail),
            Err(e) => {
                tracing::warn!(job_id = %avail.job_id, error = %e, "failed to retire unmatchable job");
                failures += 1;
                survivors.push(avail);
            }
        }
    }
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
    avail: &AvailKey,
    bodies: &mut HashMap<String, Value>,
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
        std::future::ready(format!(
            "No client can ever be eligible for this job: {rule}"
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

/// Escalate every `avail/` job whose closed `clients` roster is fully denied to
/// a synthetic `"system"` failure. The sole owner of this rule: the submit path
/// only records the denial, so an exhausted roster would otherwise sit forever.
///
/// A vanished entry survives: unlike an expired one it was likely just claimed.
async fn escalate_all_denied_jobs(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    all_keys: Vec<AvailKey>,
    bodies: &mut HashMap<String, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<AvailKey>, HashSet<JobId>, usize)> {
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

/// Fetch `avail`'s job body through the run's shared cache. `Ok(None)` means
/// the entry left `avail/` since the listing.
pub(crate) async fn cached_body(
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

/// Returns `true` when the job was positively removed.
async fn escalate_one_if_all_denied(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    avail: &AvailKey,
    denied: &HashSet<ClientId>,
    bodies: &mut HashMap<String, Value>,
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
    let failure_reason = async {
        let denials: Vec<DeniedClient> =
            stream::iter(members.map(|id| denied_client(todo, &avail.job_id, id)))
                .buffered(STORAGE_CONCURRENCY)
                .collect()
                .await;
        summarize_denials(&denials)
    };

    resolve_job_terminal(
        todo,
        submissions,
        catalog,
        &QueueEntry::Avail(avail),
        &body,
        failure_reason,
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
    Avail(&'a AvailKey),
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
            Self::Avail(avail) => todo.delete_avail(&avail.job_id, avail.expires_at).await,
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
    failure_reason: impl Future<Output = String>,
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
    match record_system_failure(
        submissions,
        catalog,
        body,
        failure_reason.await,
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
    avail: &AvailKey,
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
        std::future::ready(format!(
            "Job expired at {} before any client completed it",
            deadline.to_rfc3339_opts(SecondsFormat::Secs, true)
        )),
        "expired",
        FailureSource::Expired,
        now,
    )
    .await?;
    Ok(true)
}

/// Every `avail/` key the queue holds, decoded once. `skipped` lets a report
/// state its denominator.
pub struct AvailListing {
    pub keys: Vec<AvailKey>,
    pub skipped: usize,
}

/// List every `avail/` key in key order, decoding each once. An unparseable
/// filename is logged and dropped as absent.
pub async fn collect_all_avail(todo: &dyn TodoStore) -> anyhow::Result<AvailListing> {
    let keys = todo.list_avail().await?;
    // An unmigrated key would drop below as cruft, taking a live job out of the
    // GC live set.
    pipette_store::reject_unmigrated_avail_keys(keys.iter().map(String::as_str))?;
    Ok(keys.into_iter().fold(
        AvailListing {
            keys: Vec::new(),
            skipped: 0,
        },
        |mut listing, key| {
            match parse_avail_filename(&key) {
                Ok((job_id, expires_at)) => listing.keys.push(AvailKey {
                    key,
                    job_id,
                    expires_at,
                }),
                Err(_) => {
                    tracing::warn!(key = %key, "skipping unparseable avail filename");
                    listing.skipped += 1;
                }
            }
            listing
        },
    ))
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

/// Index every `avail/` key absent from the persisted indexed set against the
/// workers, then rewrite the set; a key, once indexed, is never paid for again.
///
/// Key order means nothing: `Uuid::now_v7` orders ids only within one process.
/// The set keeps the leased keys, so a recycled lease is not re-indexed, and is
/// written after the markers, so a crash re-indexes.
async fn index_new_jobs(
    todo: &dyn TodoStore,
    workers: &[&Client],
    tags: &FleetTags,
    all_keys: &[AvailKey],
    leased_avail_keys: &HashSet<String>,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<()> {
    let indexed = todo.read_eligible_indexed().await?;
    let new_keys: Vec<&AvailKey> = all_keys
        .iter()
        .filter(|a| !indexed.contains(&a.key))
        .collect();
    let mut markers = 0usize;
    for avail in &new_keys {
        let Some(body) = cached_body(todo, avail, bodies).await? else {
            continue;
        };
        for client in workers {
            if job_matches_client(&body, client, tags) {
                todo.write_eligible(&client.client_id, &avail.job_id, avail.expires_at)
                    .await?;
                markers += 1;
            }
        }
    }

    // Skip the PUT when the retained set already matches, so an idle queue
    // writes nothing.
    let retained: HashSet<String> = all_keys
        .iter()
        .map(|a| a.key.clone())
        .chain(leased_avail_keys.iter().cloned())
        .collect();
    if retained != indexed {
        todo.write_eligible_indexed(&retained).await?;
    }
    if !new_keys.is_empty() {
        tracing::info!(
            new_jobs = new_keys.len(),
            markers_written = markers,
            "queue-maintenance: indexed new avail jobs"
        );
    }
    Ok(())
}

/// Re-evaluate each flagged client, and each joiner, against the whole `avail/`
/// set; a flagged non-worker gets its markers cleared instead.
///
/// Reads the record fresh and deletes only the flags captured before it, so a
/// newer reindex request is never consumed. Leased jobs are flagged into
/// `pending-reindex-jobs/` before the wipe, so a crash never loses one.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds"
)]
async fn reindex_flagged_clients(
    todo: &dyn TodoStore,
    auth: &dyn ClientStore,
    tag_store: &dyn TagStore,
    workers: &HashSet<ClientId>,
    joined: &BTreeSet<ClientId>,
    all_keys: &[AvailKey],
    leased_ids: &HashSet<JobId>,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<()> {
    let flags = todo.list_pending_reindex().await?;
    // One rebuild consumes all of a client's flag keys; a joiner has none.
    let by_client: BTreeMap<ClientId, Vec<String>> = flags.into_iter().fold(
        joined.iter().map(|c| (c.clone(), Vec::new())).collect(),
        |mut m, (c, key)| {
            m.entry(c).or_default().push(key);
            m
        },
    );
    if by_client.is_empty() {
        return Ok(());
    }

    // Only a rebuild misses the leased jobs; a non-worker's cleanup drops its
    // whole partition, leased jobs included.
    if by_client
        .keys()
        .any(|client_id| workers.contains(client_id))
    {
        // Sorted so a failing write stops at the same job on every run.
        let mut leased: Vec<&JobId> = leased_ids.iter().collect();
        leased.sort_unstable();
        for job_id in leased {
            todo.write_pending_reindex_job(job_id).await?;
        }
        if !leased_ids.is_empty() {
            tracing::info!(
                jobs_flagged = leased_ids.len(),
                "queue-maintenance: flagged leased jobs for deferred reindex"
            );
        }
    }

    for (client_id, keys) in &by_client {
        // `claim` refuses a non-worker, so its markers are dead; a later join
        // rebuilds them.
        let is_worker = workers.contains(client_id);
        let record = if is_worker {
            auth.get_client(client_id).await?
        } else {
            None
        };
        let Some(client) = record else {
            // Only the captured flags: a racing re-registration's flag survives.
            todo.delete_eligible_for_client(client_id).await?;
            for key in keys {
                todo.delete_pending_reindex(key).await?;
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
        let mut markers = 0usize;
        for avail in all_keys {
            let Some(body) = cached_body(todo, avail, bodies).await? else {
                // Not evaluable now; defer it like the leased jobs above.
                todo.write_pending_reindex_job(&avail.job_id).await?;
                tracing::debug!(
                    job_id = %avail.job_id,
                    client_id = %client_id,
                    "queue-maintenance: flagged a job that left avail/ mid-rebuild for deferred reindex"
                );
                continue;
            };
            if job_matches_client(&body, &client, &tags) {
                todo.write_eligible(client_id, &avail.job_id, avail.expires_at)
                    .await?;
                markers += 1;
            }
        }
        tracing::info!(
            client_id = %client_id,
            markers_written = markers,
            "queue-maintenance: reindexed client after a matching-input change"
        );

        // Keep the flags if the record changed mid-rebuild: a `PATCH` whose own
        // flag write failed has no other repair trigger. A failed read keeps them.
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
                    "queue-maintenance: recheck read failed; keeping this client's reindex flags"
                );
                false
            }
        };
        if !record_unchanged {
            tracing::info!(
                client_id = %client_id,
                flags_retained = keys.len(),
                "queue-maintenance: client's record or tags changed during rebuild; kept its reindex flags for the next run"
            );
            continue;
        }
        for key in keys {
            todo.delete_pending_reindex(key).await?;
        }
    }
    Ok(())
}

/// Re-match each flagged job that is back in `avail/` against every client,
/// writing or deleting each marker, then clear the flag. Returns the number of
/// client records read.
///
/// Reads clients fresh: [`reindex_flagged_clients`] may have consumed the flag
/// that would repair markers from an older record.
async fn settle_pending_reindex_jobs(
    todo: &dyn TodoStore,
    auth: &dyn ClientStore,
    tag_store: &dyn TagStore,
    submissions: &dyn SubmissionStore,
    workers: &HashSet<ClientId>,
    all_keys: &[AvailKey],
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<usize> {
    let flags = todo.list_pending_reindex_jobs().await?;
    if flags.is_empty() {
        return Ok(0);
    }
    let clients = drain_clients(auth).await?;
    warn_unreadable_clients(&clients.unreadable);
    let drained = clients.items.len() + clients.unreadable.len();
    let clients = clients.items;
    let tags = FleetTags::new(tag_store.list_forward_tag_markers().await?);
    let avail_by_job: HashMap<&JobId, &AvailKey> =
        all_keys.iter().map(|a| (&a.job_id, a)).collect();

    for job_id in &flags {
        let Some(avail) = avail_by_job.get(job_id) else {
            if submissions.has_record(job_id).await? {
                todo.delete_pending_reindex_job(job_id).await?;
                tracing::info!(
                    job_id = %job_id,
                    "queue-maintenance: cleared deferred-reindex flag for completed job"
                );
            }
            continue;
        };
        let Some(body) = cached_body(todo, avail, bodies).await? else {
            // Claimed since the listing; settle on a later run.
            continue;
        };
        let mut markers = 0usize;
        for client in &clients {
            if workers.contains(&client.client_id) && job_matches_client(&body, client, &tags) {
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
    Ok(drained)
}

/// Whether to act on an orphaned marker now: its `key` was staged last run, or
/// its `owning_job` was removed this run. A first sighting is staged in `next`.
pub(crate) fn confirm_or_stage(
    prior: &HashSet<String>,
    terminal_jobs: &HashSet<JobId>,
    next: &mut HashSet<String>,
    key: String,
    owning_job: Option<&JobId>,
) -> bool {
    confirm_or_stage_group(prior, terminal_jobs, next, vec![key], owning_job)
}

/// [`confirm_or_stage`] for a bulk delete: every key staged last run, or
/// `owning_job` removed this run.
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

/// Collect the markers of jobs and clients that no longer exist, gated by
/// [`confirm_or_stage`]. Returns the next run's candidate set.
///
/// Fails fast, unlike the per-item passes: it only reclaims inert detritus, and
/// its likely failure is an S3 outage that retrying would worsen.
#[expect(
    clippy::too_many_arguments,
    reason = "the stores, the listings shared with the backlog count, and the live views \
              are each context the caller already holds"
)]
async fn reconcile_orphans(
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    eligible: &[(ClientId, JobId, ExpiresAt)],
    denied: &[(JobId, ClientId)],
    live_jobs: &HashSet<JobId>,
    live_clients: &HashSet<ClientId>,
    avail_expiries: &HashMap<JobId, ExpiresAt>,
    terminal_jobs: &HashSet<JobId>,
    prior: &HashSet<String>,
) -> anyhow::Result<HashSet<String>> {
    let mut next = HashSet::new();

    // ── eligible/clients/{client}/{job}_{exp} ── orphan iff job or client dead,
    // or the marker names a deadline the job's `avail/` entry does not carry.
    let mut dead_client_eligible: BTreeMap<ClientId, Vec<(String, JobId, ExpiresAt)>> =
        BTreeMap::new();
    let mut eligible_removed = 0usize;
    let mut stale_expiry_removed = 0usize;
    for (client_id, job_id, expires_at) in eligible {
        let client_dead = !live_clients.contains(client_id);
        let job_dead = !live_jobs.contains(job_id);
        // A re-date's residue, provably stale, so it goes on first sighting. A
        // leased job has no `avail_expiries` entry, so its markers are safe.
        let stale_expiry = avail_expiries
            .get(job_id)
            .is_some_and(|current| current != expires_at);
        if !client_dead && !job_dead && !stale_expiry {
            continue;
        }
        let key = format!(
            "eligible/clients/{}/{}",
            client_id,
            eligible_filename(job_id, *expires_at)
        );
        if client_dead {
            dead_client_eligible
                .entry(client_id.clone())
                .or_default()
                .push((key, job_id.clone(), *expires_at));
        } else if stale_expiry {
            todo.delete_eligible(client_id, job_id, *expires_at).await?;
            stale_expiry_removed += 1;
        } else if confirm_or_stage(prior, terminal_jobs, &mut next, key, Some(job_id)) {
            todo.delete_eligible(client_id, job_id, *expires_at).await?;
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

    // ── denied/{job}.{client} ── orphan iff job or client dead.
    let mut dead_job_denied: BTreeMap<JobId, Vec<(String, ClientId)>> = BTreeMap::new();
    let mut denied_removed = 0usize;
    for (job_id, client_id) in denied {
        let job_dead = !live_jobs.contains(job_id);
        let client_dead = !live_clients.contains(client_id);
        if !job_dead && !client_dead {
            continue;
        }
        let key = format!("denied/{}", denied_marker_filename(job_id, client_id));
        if job_dead {
            dead_job_denied
                .entry(job_id.clone())
                .or_default()
                .push((key, client_id.clone()));
        } else if confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            todo.delete_denied(job_id, client_id).await?;
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

    // ── suspended/{client}.json ── orphan iff client dead; `conflicting_job_id`
    // is not an owner.
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

    // ── liveness/{client} ── orphan iff client dead; never collected by age.
    let mut liveness_removed = 0usize;
    for (client_id, _seen_at) in todo.list_liveness().await? {
        if live_clients.contains(&client_id) {
            continue;
        }
        let key = format!("liveness/{}", client_id);
        if confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            todo.delete_liveness(&client_id).await?;
            liveness_removed += 1;
            tracing::info!(
                client_id = %client_id,
                "queue-maintenance: deleted orphaned liveness marker for deleted client"
            );
        }
    }

    // ── pending-reindex/{client}.{uuid} ── orphan iff client dead.
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

    // ── pending-reindex-jobs/{job} ── orphan iff job dead, such as one deleted
    // while flagged that the settle pass never clears.
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

    // ── lost/{job}/ ── orphan iff job dead.
    let mut lost_lease_jobs_removed = 0usize;
    for job_id in todo.list_lost_lease_jobs().await? {
        if live_jobs.contains(&job_id) {
            continue;
        }
        let key = format!("lost/{job_id}/");
        if confirm_or_stage(prior, terminal_jobs, &mut next, key, Some(&job_id)) {
            todo.delete_lost_leases_for_job(&job_id).await?;
            lost_lease_jobs_removed += 1;
        }
    }

    // ── leased/{client}/{job}_{exp} ── a deleted client's lease is recycled,
    // not deleted: it holds the only copy of the job body.
    let mut leases_recycled = 0usize;
    for raw_key in todo.list_leased().await? {
        let Ok(Lease {
            job_id,
            expires_at,
            client_id,
            lease_expiry,
        }) = parse_leased_key(&raw_key)
        else {
            continue;
        };
        if live_clients.contains(&client_id) {
            continue;
        }
        let key = format!("leased/{}", raw_key);
        if !confirm_or_stage(prior, terminal_jobs, &mut next, key, None) {
            continue;
        }
        match resolve_or_recycle_lease(
            todo,
            submissions,
            &job_id,
            expires_at,
            &client_id,
            lease_expiry,
        )
        .await?
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
        + stale_expiry_removed
        + denied_removed
        + suspensions_removed
        + liveness_removed
        + reindex_flags_removed
        + reindex_job_flags_removed
        + lost_lease_jobs_removed
        + leases_recycled
        > 0
    {
        tracing::info!(
            eligible_removed,
            stale_expiry_removed,
            denied_removed,
            suspensions_removed,
            liveness_removed,
            reindex_flags_removed,
            reindex_job_flags_removed,
            lost_lease_jobs_removed,
            leases_recycled,
            "queue-maintenance: reconciled orphaned todo/ markers"
        );
    }
    Ok(next)
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

/// The backlog of each client: the count of its claimable markers.
fn count_backlog(claimable: &[&(ClientId, JobId, ExpiresAt)]) -> BTreeMap<ClientId, u64> {
    claimable
        .iter()
        .fold(BTreeMap::new(), |mut acc, (client_id, _, _)| {
            *acc.entry(client_id.clone()).or_default() += 1;
            acc
        })
}

/// The campaigns that hold each job: the campaigns of the plans with a
/// current membership for the job. A plan holds no job in this map if its
/// teardown started, if it is outside a campaign, or if it has no manifest.
fn job_campaigns<'a>(
    markers: &'a [JobPlanMarker],
    manifests: &'a [PlanManifest],
    cancel_markers: &HashSet<PlanId>,
    stranded: &HashSet<PlanId>,
    now: DateTime<Utc>,
) -> HashMap<&'a JobId, BTreeSet<&'a CampaignId>> {
    let plan_campaigns: HashMap<&PlanId, &CampaignId> = manifests
        .iter()
        .filter(|manifest| {
            !cancel_markers.contains(&manifest.plan_id)
                && !stranded.contains(&manifest.plan_id)
                && !matches!(
                    manifest.status,
                    PlanStatus::Cancelling | PlanStatus::Cancelled
                )
        })
        .filter_map(|manifest| Some((&manifest.plan_id, manifest.campaign_id.as_ref()?)))
        .collect();
    markers
        .iter()
        .filter(|marker| !marker.expires_at.is_expired(now))
        .filter_map(|marker| Some((&marker.job_id, *plan_campaigns.get(&marker.plan_id)?)))
        .fold(HashMap::new(), |mut acc, (job_id, campaign_id)| {
            acc.entry(job_id)
                .or_insert_with(BTreeSet::new)
                .insert(campaign_id);
            acc
        })
}

/// Count the backlog of each client by campaign. A claimable marker counts
/// once in each campaign that holds its job. If no campaign holds the job, the
/// marker counts once in `outside_campaigns`.
fn count_campaign_backlog(
    claimable: &[&(ClientId, JobId, ExpiresAt)],
    job_campaigns: &HashMap<&JobId, BTreeSet<&CampaignId>>,
) -> BTreeMap<ClientId, CampaignBacklog> {
    claimable.iter().fold(
        BTreeMap::new(),
        |mut acc: BTreeMap<ClientId, CampaignBacklog>, (client_id, job_id, _)| {
            let backlog = acc.entry(client_id.clone()).or_default();
            match job_campaigns.get(job_id) {
                Some(campaigns) => campaigns.iter().for_each(|campaign_id| {
                    *backlog
                        .by_campaign
                        .entry((*campaign_id).clone())
                        .or_default() += 1;
                }),
                None => backlog.outside_campaigns += 1,
            }
            acc
        },
    )
}

pub(crate) fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Delete `tmp/` partial job files older than `max_age`, debris of a crashed
/// planner.
async fn cleanup_stale_tmp(todo: &dyn TodoStore, max_age: Duration) -> anyhow::Result<()> {
    let stale = todo.list_stale_tmp(max_age).await?;
    if stale.is_empty() {
        return Ok(());
    }
    for key in &stale {
        todo.delete_tmp_object(key).await?;
    }
    tracing::info!(
        files_deleted = stale.len(),
        "queue-maintenance: deleted stale tmp/ files"
    );
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
    use rstest::rstest;

    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_core::config::StorageConfig;
    use pipette_core::denial::DenialRecord;
    use pipette_core::todo_filename::leased_key;
    use pipette_plan_types::ClientId;
    use pipette_store::ClientStore;
    use pipette_store_fs::{
        LocalFsSubmissionStore, LocalFsTagStore, build_local_fs_client_store,
        build_local_fs_todo_store,
    };

    use super::*;

    /// A `list_objects_v2` rule that answers like a bucket holding `keys`.
    ///
    /// Duplicated from `pipette-store-s3`: a shared crate would put
    /// `aws-smithy-mocks` in `THIRD-PARTY-LICENSES.md`.
    fn bucket_list_rule(keys: &'static [&'static str]) -> aws_smithy_mocks::Rule {
        aws_smithy_mocks::mock!(aws_sdk_s3::Client::list_objects_v2).then_compute_output(
            move |req| {
                let prefix = req.prefix().unwrap_or_default().to_string();
                let contents: Vec<aws_sdk_s3::types::Object> = keys
                    .iter()
                    .filter(|key| key.starts_with(&prefix))
                    .map(|key| aws_sdk_s3::types::Object::builder().key(*key).build())
                    .collect();
                aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
                    .set_contents(Some(contents))
                    .is_truncated(false)
                    .build()
            },
        )
    }

    /// A directory bucket answers each `list_avail` call with the whole prefix,
    /// so every extra call re-reads every key.
    #[tokio::test]
    async fn a_sweep_lists_the_avail_prefix_once() -> anyhow::Result<()> {
        const BUCKET: &[&str] = &[
            "todo/avail/job1/never.json",
            "todo/avail/job2/never.json",
            "todo/avail/job3/20260908T000000Z.json",
        ];
        let list = bucket_list_rule(BUCKET);
        let todo = pipette_store_s3::S3TodoStore::new(
            aws_smithy_mocks::mock_client!(
                aws_sdk_s3,
                aws_smithy_mocks::RuleMode::MatchAny,
                &[&list]
            ),
            "test-bucket",
            "",
        );
        let swept = collect_all_avail(&todo).await?;
        assert_eq!(swept.keys.len(), BUCKET.len(), "the sweep sees every entry");
        assert_eq!(swept.skipped, 0, "every entry parsed");
        assert_eq!(list.num_calls(), 1, "the sweep lists `avail/` once");
        Ok(())
    }

    /// An expired lease whose job has a record is deleted, not recycled, and
    /// reported as resolved.
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
                ExpiresAt::Never,
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
        assert_eq!(todo.list_leased().await?.len(), 1);
        Ok(())
    }

    /// A record can be tens of megabytes. The counters are process-wide, so
    /// this needs its own process, as `cargo nextest` gives it.
    #[tokio::test]
    async fn recycle_pass_decodes_no_submission_body() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = build_local_fs_todo_store(&StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        })?;
        let submissions = pipette_store_s3::in_memory_submission_store();
        let leased_dir = dir.path().join("todo").join("leased");

        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let client = ClientId::try_new("client1")?;
        ["job-unrecorded", "job-recorded"]
            .into_iter()
            .try_for_each(|job| -> anyhow::Result<()> {
                let path = leased_dir.join(leased_key(
                    &JobId::new_unchecked(job),
                    ExpiresAt::Never,
                    &client,
                    "2026-01-01T00:00:00Z".parse()?,
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

        let start = S3_DECODE.checkpoint();
        let (_, resolved, failures) =
            settle_expired_leases(&*todo, &*submissions, &HashMap::new(), 3, now).await?;

        assert_eq!(failures, 0);
        assert_eq!(resolved, HashSet::from([recorded]));
        assert_eq!(S3_DECODE.checkpoint().since(start).bodies, 0);
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

    /// Both directions: the stale marker is deleted, the missing one written. A
    /// client outside the workers loses its marker even though it matches.
    #[tokio::test]
    async fn settle_matches_against_current_client_records() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        };
        let todo = build_local_fs_todo_store(&config)?;
        let auth = build_local_fs_client_store(&config)?;
        let submissions = LocalFsSubmissionStore::new(dir.path().join("submissions"));

        let mismatched = settle_test_client("settle-mismatch", "00", "chip-b")?;
        let matched = settle_test_client("settle-match", "11", "chip-a")?;
        let outsider = settle_test_client("settle-outsider", "99", "chip-a")?;
        auth.put_client(&mismatched).await?;
        auth.put_client(&matched).await?;
        auth.put_client(&outsider).await?;

        let job = JobId::new_unchecked("job-settle");
        todo.write_eligible(&mismatched.client_id, &job, ExpiresAt::Never)
            .await?;
        todo.write_eligible(&outsider.client_id, &job, ExpiresAt::Never)
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

        let tags = LocalFsTagStore::new(dir.path().to_path_buf());
        let workers = HashSet::from([mismatched.client_id.clone(), matched.client_id.clone()]);
        settle_pending_reindex_jobs(
            &*todo,
            &*auth,
            &tags,
            &submissions,
            &workers,
            &[avail],
            &mut bodies,
        )
        .await?;

        assert_eq!(
            todo.list_all_eligible().await?,
            vec![(matched.client_id.clone(), job.clone(), ExpiresAt::Never)]
        );
        assert!(todo.list_pending_reindex_jobs().await?.is_empty());
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

    /// One `reindex_flagged_clients` call over a todo store in a tempdir; the
    /// inputs a test does not set start empty.
    struct Reindex<T = LocalFsTagStore> {
        _dir: tempfile::TempDir,
        todo: std::sync::Arc<dyn TodoStore>,
        tags: T,
        workers: HashSet<ClientId>,
        joined: BTreeSet<ClientId>,
        keys: Vec<AvailKey>,
        leased: HashSet<JobId>,
        bodies: HashMap<String, Value>,
    }

    impl Reindex {
        fn new() -> anyhow::Result<Self> {
            let dir = tempfile::tempdir()?;
            let tags = LocalFsTagStore::new(dir.path().to_path_buf());
            Self::in_dir(dir, tags)
        }
    }

    impl<T: TagStore> Reindex<T> {
        fn with_tags(tags: T) -> anyhow::Result<Self> {
            Self::in_dir(tempfile::tempdir()?, tags)
        }

        fn in_dir(dir: tempfile::TempDir, tags: T) -> anyhow::Result<Self> {
            let todo = build_local_fs_todo_store(&StorageConfig::LocalFs {
                data_dir: dir.path().to_path_buf(),
            })?;
            Ok(Self {
                _dir: dir,
                todo,
                tags,
                workers: HashSet::new(),
                joined: BTreeSet::new(),
                keys: Vec::new(),
                leased: HashSet::new(),
                bodies: HashMap::new(),
            })
        }

        async fn run(&mut self, auth: &dyn ClientStore) -> anyhow::Result<()> {
            reindex_flagged_clients(
                &*self.todo,
                auth,
                &self.tags,
                &self.workers,
                &self.joined,
                &self.keys,
                &self.leased,
                &mut self.bodies,
            )
            .await
        }
    }

    /// A record that still matches at the recheck consumes the flags as usual.
    #[rstest]
    #[case::record_changed(true)]
    #[case::record_unchanged(false)]
    #[tokio::test]
    async fn reindex_keeps_captured_flags_when_record_changes_mid_rebuild(
        #[case] changed: bool,
    ) -> anyhow::Result<()> {
        let mut r = Reindex::new()?;

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
        let avail = AvailKey {
            key: "avail/job-recheck".into(),
            job_id: job.clone(),
            expires_at: ExpiresAt::Never,
        };
        r.bodies.insert(
            avail.key.clone(),
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
        let mut r = Reindex::new()?;

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
        let mut r = Reindex::new()?;
        let client = settle_test_client("tagged-client", "44", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        r.tags
            .add_client_tag(&client.client_id, &pipette_plan_types::Tag::try_new("lab")?)
            .await?;
        r.todo.write_pending_reindex(&client.client_id).await?;

        let tagged_job = JobId::new_unchecked("job-tagged");
        r.keys = vec![
            AvailKey {
                key: "avail/job-tagged".into(),
                job_id: tagged_job.clone(),
                expires_at: ExpiresAt::Never,
            },
            AvailKey {
                key: "avail/job-other-tag".into(),
                job_id: JobId::new_unchecked("job-other-tag"),
                expires_at: ExpiresAt::Never,
            },
        ];
        r.bodies = HashMap::from([
            (
                "avail/job-tagged".to_string(),
                serde_json::json!({"job_id": "job-tagged", "requires": ["tag:lab"]}),
            ),
            (
                "avail/job-other-tag".to_string(),
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

    /// No record read, and no leased job flagged: the cleanup covers them.
    #[tokio::test]
    async fn reindex_clears_a_flagged_client_that_is_not_a_worker() -> anyhow::Result<()> {
        let mut r = Reindex::new()?;
        let auth = ScriptedClientStore::new([]);
        let client = ClientId::try_new("ev1_pending")?;
        let job = JobId::new_unchecked("job-held");
        r.todo
            .write_eligible(&client, &job, ExpiresAt::Never)
            .await?;
        r.todo.write_pending_reindex(&client).await?;

        r.leased.insert(job);
        r.run(&auth).await?;
        auth.unexpected.none()?;

        assert!(r.todo.list_all_eligible().await?.is_empty());
        assert!(r.todo.list_pending_reindex().await?.is_empty());
        assert!(r.todo.list_pending_reindex_jobs().await?.is_empty());
        Ok(())
    }

    /// The leased jobs are still flagged, because the client counted as a worker.
    #[tokio::test]
    async fn reindex_clears_a_flagged_worker_deleted_mid_run() -> anyhow::Result<()> {
        let mut r = Reindex::new()?;
        let auth = ScriptedClientStore::new([Ok(None)]);
        let client = ClientId::try_new("ev1_deleted")?;
        let job = JobId::new_unchecked("job-held");
        let leased = JobId::new_unchecked("job-leased");
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
        assert_eq!(r.todo.list_pending_reindex_jobs().await?, vec![leased]);
        Ok(())
    }

    /// A joiner gets no `pending-reindex/` flag; unchanged workers return none.
    #[tokio::test]
    async fn worker_changes_return_joiners_and_clear_leavers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let todo = build_local_fs_todo_store(&StorageConfig::LocalFs {
            data_dir: dir.path().to_path_buf(),
        })?;
        let stays = ClientId::try_new("ev1_stays")?;
        let leaves = ClientId::try_new("ev1_leaves")?;
        let joins = ClientId::try_new("ev1_joins")?;
        let job = JobId::new_unchecked("job-held");
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

    /// The leased jobs are flagged for a deferred settle as for any rebuild.
    #[tokio::test]
    async fn reindex_rebuilds_a_joiner_without_a_flag() -> anyhow::Result<()> {
        let mut r = Reindex::new()?;
        let client = settle_test_client("joined-client", "55", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        let stale = JobId::new_unchecked("job-stale");
        let leased = JobId::new_unchecked("job-leased");
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
        assert_eq!(r.todo.list_pending_reindex_jobs().await?, vec![leased]);
        Ok(())
    }

    #[tokio::test]
    async fn reindex_keeps_captured_flags_when_tags_change_mid_rebuild() -> anyhow::Result<()> {
        let client = settle_test_client("retagged-client", "55", "chip-a")?;
        let auth = ScriptedClientStore::new([Ok(Some(client.clone())), Ok(Some(client.clone()))]);
        // The rebuild reads no tag; the recheck reads the one written since.
        let mut r = Reindex::with_tags(ScriptedTagStore::new([
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::from([pipette_plan_types::Tag::try_new("lab")?]),
        ]))?;
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

    // ── backlog count ──────────────────────────────────────────────────────

    fn backlog_now() -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(1_800_000_000)
    }

    fn deadline(offset: i64) -> ExpiresAt {
        ExpiresAt::At(backlog_now() + chrono::Duration::seconds(offset))
    }

    /// The backlog that the claim filters leave, counted at [`backlog_now`].
    fn backlog_of(
        eligible: &[(ClientId, JobId, ExpiresAt)],
        avail_expiries: &HashMap<JobId, ExpiresAt>,
        denied: &[(JobId, ClientId)],
    ) -> BTreeMap<ClientId, u64> {
        count_backlog(&claimable_markers(
            eligible,
            avail_expiries,
            denied,
            backlog_now(),
        ))
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
    fn count_backlog_applies_the_claim_filters(
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

    /// A client with no counting marker has no entry.
    #[test]
    fn count_backlog_sums_each_clients_markers() -> anyhow::Result<()> {
        let (a, b) = (ClientId::try_new("ev1_a")?, ClientId::try_new("ev1_b")?);
        let (j1, j2) = (JobId::new_unchecked("job-1"), JobId::new_unchecked("job-2"));
        let eligible = vec![
            (a.clone(), j1.clone(), deadline(60)),
            (a.clone(), j2.clone(), deadline(60)),
            (b.clone(), j1.clone(), deadline(60)),
        ];
        let avail_expiries =
            HashMap::from([(j1.clone(), deadline(60)), (j2.clone(), deadline(60))]);
        let denied = vec![(j1, b)];
        assert_eq!(
            backlog_of(&eligible, &avail_expiries, &denied),
            BTreeMap::from([(a, 2)])
        );
        Ok(())
    }

    fn backlog_plan(n: u8) -> anyhow::Result<PlanId> {
        Ok(PlanId::try_new(format!(
            "plan-00000000-0000-0000-0000-00000000000{n}"
        ))?)
    }

    fn backlog_campaign(n: u8) -> anyhow::Result<CampaignId> {
        Ok(CampaignId::try_new(format!(
            "campaign_00000000-0000-0000-0000-00000000000{n}"
        ))?)
    }

    /// The manifest of plan `plan`, in campaign `campaign` if one is given.
    fn backlog_manifest(
        plan: u8,
        campaign: Option<u8>,
        status: PlanStatus,
    ) -> anyhow::Result<PlanManifest> {
        Ok(PlanManifest {
            campaign_id: campaign.map(backlog_campaign).transpose()?,
            ..pipette_core::plan::sample_manifest(backlog_plan(plan)?, status, false)?
        })
    }

    /// The `job-plans/` marker of plan `plan` for `job`.
    fn backlog_marker(
        job: &JobId,
        plan: u8,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<JobPlanMarker> {
        Ok(JobPlanMarker {
            job_id: job.clone(),
            plan_id: backlog_plan(plan)?,
            expires_at,
            written_at: backlog_now(),
        })
    }

    /// One claimable marker of `ev1_a` for `job-1`. The cases give:
    /// - `memberships`: the `job-plans/` markers of the job.
    /// - `plans`: the manifests, each with a plan, a campaign and a status.
    /// - `cancelled`: the plans that have a cancel marker.
    /// - `stranded`: the plans that teardown ended in `creating`.
    #[rstest]
    #[case::no_membership(&[], &[], &[], &[], &[], 1)]
    #[case::one_campaign(&[(1, deadline(60))], &[(1, Some(1), PlanStatus::Active)], &[], &[], &[(1, 1)], 0)]
    #[case::two_plans_of_one_campaign_count_once(
        &[(1, deadline(60)), (2, ExpiresAt::Never)],
        &[(1, Some(1), PlanStatus::Active), (2, Some(1), PlanStatus::Active)],
        &[],
        &[],
        &[(1, 1)],
        0
    )]
    #[case::two_campaigns_count_once_each(
        &[(1, deadline(60)), (2, deadline(60))],
        &[(1, Some(1), PlanStatus::Active), (2, Some(2), PlanStatus::PendingClients)],
        &[],
        &[],
        &[(1, 1), (2, 1)],
        0
    )]
    #[case::plan_outside_campaigns(&[(1, deadline(60))], &[(1, None, PlanStatus::Active)], &[], &[], &[], 1)]
    #[case::campaign_beside_an_outside_plan(
        &[(1, deadline(60)), (2, deadline(60))],
        &[(1, Some(1), PlanStatus::Active), (2, None, PlanStatus::Active)],
        &[],
        &[],
        &[(1, 1)],
        0
    )]
    #[case::lapsed_membership(&[(1, deadline(-60))], &[(1, Some(1), PlanStatus::Active)], &[], &[], &[], 1)]
    #[case::membership_without_manifest(&[(1, deadline(60))], &[], &[], &[], &[], 1)]
    #[case::cancelling_plan(&[(1, deadline(60))], &[(1, Some(1), PlanStatus::Cancelling)], &[], &[], &[], 1)]
    #[case::cancel_marker(&[(1, deadline(60))], &[(1, Some(1), PlanStatus::Active)], &[1], &[], &[], 1)]
    #[case::stranded_plan(&[(1, deadline(60))], &[(1, Some(1), PlanStatus::Creating)], &[], &[1], &[], 1)]
    fn count_campaign_backlog_counts_each_campaign_once(
        #[case] memberships: &[(u8, ExpiresAt)],
        #[case] plans: &[(u8, Option<u8>, PlanStatus)],
        #[case] cancelled: &[u8],
        #[case] stranded: &[u8],
        #[case] by_campaign: &[(u8, u64)],
        #[case] outside_campaigns: u64,
    ) -> anyhow::Result<()> {
        let client = ClientId::try_new("ev1_a")?;
        let job = JobId::new_unchecked("job-1");
        let markers = memberships
            .iter()
            .map(|(plan, expires_at)| backlog_marker(&job, *plan, *expires_at))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let manifests = plans
            .iter()
            .map(|(plan, campaign, status)| backlog_manifest(*plan, *campaign, *status))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let cancel_markers = cancelled
            .iter()
            .map(|plan| backlog_plan(*plan))
            .collect::<anyhow::Result<HashSet<_>>>()?;
        let stranded = stranded
            .iter()
            .map(|plan| backlog_plan(*plan))
            .collect::<anyhow::Result<HashSet<_>>>()?;
        let marker = (client.clone(), job, deadline(60));
        let expected = CampaignBacklog {
            by_campaign: by_campaign
                .iter()
                .map(|(campaign, jobs)| Ok((backlog_campaign(*campaign)?, *jobs)))
                .collect::<anyhow::Result<_>>()?,
            outside_campaigns,
        };
        assert_eq!(
            count_campaign_backlog(
                &[&marker],
                &job_campaigns(
                    &markers,
                    &manifests,
                    &cancel_markers,
                    &stranded,
                    backlog_now(),
                ),
            ),
            BTreeMap::from([(client, expected)])
        );
        Ok(())
    }

    /// The count of each client uses only the markers of that client. A
    /// client with no marker has no entry.
    #[test]
    fn count_campaign_backlog_sums_each_clients_markers() -> anyhow::Result<()> {
        let (a, b) = (ClientId::try_new("ev1_a")?, ClientId::try_new("ev1_b")?);
        let (j1, j2) = (JobId::new_unchecked("job-1"), JobId::new_unchecked("job-2"));
        let markers = [&j1, &j2]
            .into_iter()
            .map(|job_id| backlog_marker(job_id, 1, ExpiresAt::Never))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let manifests = vec![backlog_manifest(1, Some(1), PlanStatus::Active)?];
        let claimable = [
            (a.clone(), j1, deadline(60)),
            (a.clone(), j2, deadline(60)),
            (b.clone(), JobId::new_unchecked("job-3"), deadline(60)),
        ];
        let claimable: Vec<_> = claimable.iter().collect();
        assert_eq!(
            count_campaign_backlog(
                &claimable,
                &job_campaigns(
                    &markers,
                    &manifests,
                    &HashSet::new(),
                    &HashSet::new(),
                    backlog_now(),
                ),
            ),
            BTreeMap::from([
                (
                    a,
                    CampaignBacklog {
                        by_campaign: BTreeMap::from([(backlog_campaign(1)?, 2)]),
                        outside_campaigns: 0,
                    }
                ),
                (
                    b,
                    CampaignBacklog {
                        by_campaign: BTreeMap::new(),
                        outside_campaigns: 1,
                    }
                ),
            ])
        );
        Ok(())
    }

    // ── orphan reconciliation ──────────────────────────────────────────────

    /// A `TodoStore` + `SubmissionStore` pair over one tempdir.
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

    /// Seed one lease directly on disk under `ExpiresAt::Never`.
    fn seed_lease(
        data_dir: &std::path::Path,
        job: &JobId,
        client: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let path = data_dir.join("todo").join("leased").join(leased_key(
            job,
            ExpiresAt::Never,
            client,
            expiry,
        ));
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

    /// No job was re-dated, so only job and client liveness are in play.
    fn no_redates() -> HashMap<JobId, ExpiresAt> {
        HashMap::new()
    }

    async fn sweep(
        todo: &dyn TodoStore,
        submissions: &dyn SubmissionStore,
        live_jobs: &HashSet<JobId>,
        live_clients: &HashSet<ClientId>,
        avail_expiries: &HashMap<JobId, ExpiresAt>,
        terminal_jobs: &HashSet<JobId>,
        prior: &HashSet<String>,
    ) -> anyhow::Result<HashSet<String>> {
        reconcile_orphans(
            todo,
            submissions,
            &todo.list_all_eligible().await?,
            &todo.list_all_denied().await?,
            live_jobs,
            live_clients,
            avail_expiries,
            terminal_jobs,
            prior,
        )
        .await
    }

    /// A dead client's markers go on the second sweep, its lease recycled; a
    /// live client's stay. Every job is live, so only the roster matters.
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
        todo.write_denied(&job, &client, &DenialRecord::default())
            .await?;
        todo.write_suspension(&client, "2026-06-01T00:00:00Z".parse()?, &job)
            .await?;
        todo.stamp_liveness(&client).await?;
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

        // First sweep never removes anything.
        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_all_eligible().await?.len(), 1);
        assert_eq!(todo.list_all_denied().await?.len(), 1);
        assert_eq!(todo.list_suspensions().await?.len(), 1);
        assert_eq!(todo.list_liveness().await?.len(), 1);
        assert_eq!(todo.list_pending_reindex().await?.len(), 1);
        assert_eq!(todo.list_leased().await?.len(), 1);
        assert_eq!(candidates.len(), if client_live { 0 } else { 6 });

        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &candidates,
        )
        .await?;
        let remaining = usize::from(client_live);
        assert_eq!(todo.list_all_eligible().await?.len(), remaining);
        assert_eq!(todo.list_all_denied().await?.len(), remaining);
        assert_eq!(todo.list_suspensions().await?.len(), remaining);
        assert_eq!(todo.list_liveness().await?.len(), remaining);
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
        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_leased().await?.len(), 1);
        assert_eq!(candidates.len(), 1);

        // Second sweep deletes the stale lease; the recorded job is not recycled.
        sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &candidates,
        )
        .await?;
        assert!(todo.list_leased().await?.is_empty());
        assert!(todo.get_avail_by_job(&job).await?.is_none());
        Ok(())
    }

    /// The per-marker denied delete spares a live job's denials from live
    /// clients.
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
        todo.write_denied(&live_job, &dead_client, &DenialRecord::default())
            .await?;
        todo.write_denied(&live_job, &live_client, &DenialRecord::default())
            .await?;

        let live_jobs = HashSet::from([live_job.clone()]);
        let live_clients = HashSet::from([live_client.clone()]);
        let terminal: HashSet<JobId> = HashSet::new();

        // Two sweeps to clear the two-sighting gate.
        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
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

    #[tokio::test]
    async fn reconcile_sweeps_terminal_job_markers_same_run() -> anyhow::Result<()> {
        let (todo, submissions, _dir) = recon_stores()?;
        let client = ClientId::try_new("live-client")?;
        let job = JobId::new_unchecked("job-terminal");

        todo.write_eligible(&client, &job, ExpiresAt::Never).await?;
        todo.write_denied(&job, &client, &DenialRecord::default())
            .await?;

        let live_jobs: HashSet<JobId> = HashSet::new();
        let live_clients = HashSet::from([client.clone()]);
        let terminal = HashSet::from([job.clone()]);

        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert!(todo.list_all_eligible().await?.is_empty());
        assert!(todo.list_all_denied().await?.is_empty());
        assert!(candidates.is_empty());
        Ok(())
    }

    /// Its marker is neither deleted nor re-staged.
    #[tokio::test]
    async fn reconcile_spares_reappearing_client() -> anyhow::Result<()> {
        let (todo, submissions, _dir) = recon_stores()?;
        let client = ClientId::try_new("flapper")?;
        let job = JobId::new_unchecked("job-x");
        todo.write_eligible(&client, &job, ExpiresAt::Never).await?;

        let live_jobs = HashSet::from([job.clone()]);
        let terminal: HashSet<JobId> = HashSet::new();

        // First sweep: client absent → the marker is staged.
        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &HashSet::new(),
            &no_redates(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_all_eligible().await?.len(), 1);
        assert_eq!(candidates.len(), 1);

        // Second sweep: the client reappeared → spared, candidate cleared.
        let live_clients = HashSet::from([client.clone()]);
        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &candidates,
        )
        .await?;
        assert_eq!(todo.list_all_eligible().await?.len(), 1);
        assert!(candidates.is_empty());
        Ok(())
    }

    /// A live job's flag is spared.
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
        let candidates = sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
            &terminal,
            &HashSet::new(),
        )
        .await?;
        assert_eq!(todo.list_pending_reindex_jobs().await?.len(), 2);

        // Second sweep collects the dead job's flag; the live job's is spared.
        sweep(
            &*todo,
            &submissions,
            &live_jobs,
            &live_clients,
            &no_redates(),
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
                        .map(|job_id| self.todo.promote_avail(job_id, ExpiresAt::Never)),
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
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&pipette_core::config::Config {
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        })?;
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
