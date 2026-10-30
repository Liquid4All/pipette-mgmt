//! The plan-level passes of `pipette-mgmt queue-maintenance`, the sole writer
//! of a plan's lifecycle. See `docs/plan-ingestion.md` for the lifecycle and
//! the cancel teardown.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;

use pipette_core::client::Client;
use pipette_core::matching::{FleetTags, job_matches_client};
use pipette_core::plan::{
    ExpiredJob, PlanManifest, ProgressSnapshot, SettledOutcome, StarvedGroup, past_gate,
};
use pipette_core::todo_filename::Lease;
use pipette_plan_types::{Counts, ExpiresAt, JobId, PlanId, PlanStatus};
use pipette_store::{JobPlanMarker, PlanStore, SubmissionStore, TodoStore};

use crate::plan_ingestion::requirement_key;
use crate::queue_maintenance::{AvailKey, cached_body, confirm_or_stage, millis};

/// The run's snapshot of the `job-plans/` tree, indexed by job.
///
/// Teardown only skips on it; every membership it acts on is read fresh, since
/// a stale snapshot would retire a job another plan just joined.
pub(crate) struct Membership(HashMap<JobId, Vec<(PlanId, ExpiresAt)>>);

impl Membership {
    pub(crate) fn new(markers: Vec<JobPlanMarker>) -> Self {
        Self(markers.into_iter().fold(HashMap::new(), |mut acc, marker| {
            acc.entry(marker.job_id)
                .or_default()
                .push((marker.plan_id, marker.expires_at));
            acc
        }))
    }

    /// Whether `plan_id` released this job in an earlier pass: another plan
    /// holds a membership, and `plan_id` does not.
    ///
    /// A job with no marker at all predates the membership tree, so it is not
    /// released. A lapsed member reads alike; `expired_jobs` tells them apart.
    fn released_by(&self, job_id: &JobId, plan_id: &PlanId) -> bool {
        let Some(members) = self.0.get(job_id) else {
            return false;
        };
        !members.iter().any(|(member, _)| member == plan_id)
    }
}

/// How [`tear_down_plans`] ended each plan, as [`reconcile_plans`] reads it.
///
/// One struct so the same-typed sets cannot be transposed at a call site.
pub(crate) struct PlanEndings {
    /// Per ending plan, the jobs it released rather than retired. Not in
    /// [`Self::perturbed`]: a release deletes nothing from the queue.
    pub released: HashMap<PlanId, HashSet<JobId>>,
    /// Plans this pass retired a job of; [`decide_status`] will not settle them.
    pub perturbed: HashSet<PlanId>,
    /// Plans stranded in `creating`; no cancel marker names them.
    pub stranded: HashSet<PlanId>,
}

impl PlanEndings {
    fn for_plan(&self, plan_id: &PlanId, requested: bool) -> Ending<'_> {
        Ending {
            requested,
            stranded: self.stranded.contains(plan_id),
            retired_this_pass: self.perturbed.contains(plan_id),
            released_this_pass: self.released.get(plan_id),
        }
    }
}

/// How this pass ended one plan: [`PlanEndings`] narrowed to it.
struct Ending<'a> {
    requested: bool,
    stranded: bool,
    retired_this_pass: bool,
    released_this_pass: Option<&'a HashSet<JobId>>,
}

/// What [`tear_down_plans`] left behind, for the caller to fold into the run.
pub(crate) struct TeardownOutcome {
    pub avail: Vec<AvailKey>,
    /// One list, so the GC live set and the recycle keys describe the same
    /// leases.
    pub leased: Vec<Lease>,
    /// Confirmed terminal for this run's GC sweeps, skipping the two-sighting
    /// orphan path.
    pub retired: HashSet<JobId>,
    pub endings: PlanEndings,
    pub failures: usize,
}

/// End every live job of every plan that owes teardown: latched `cancelling` or
/// `cancelled`, or stranded in `creating`.
///
/// A job is retired only when its last membership goes; otherwise it is
/// released to the plans still holding it. Lenient per job: a failure is logged,
/// counted, and retried next run, except a failed restore, which loses the job
/// (see `docs/plan-ingestion.md`).
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds; a bundle type \
              would be single-use"
)]
pub(crate) async fn tear_down_plans(
    todo: &dyn TodoStore,
    plans: &dyn PlanStore,
    manifests: &[PlanManifest],
    markers: &HashSet<PlanId>,
    membership: &Membership,
    avail: Vec<AvailKey>,
    leases: Vec<Lease>,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> anyhow::Result<TeardownOutcome> {
    let doomed: Vec<(&PlanManifest, Cause)> = manifests
        .iter()
        .filter_map(|m| teardown_cause(m, markers, now, creating_max_age).map(|cause| (m, cause)))
        .collect();
    let stranded: HashSet<PlanId> = doomed
        .iter()
        .filter(|(_, cause)| *cause == Cause::CreatingStranded)
        .map(|(m, _)| m.plan_id.clone())
        .collect();
    if doomed.is_empty() {
        return Ok(TeardownOutcome {
            avail,
            leased: leases,
            retired: HashSet::new(),
            endings: PlanEndings {
                released: HashMap::new(),
                perturbed: HashSet::new(),
                stranded,
            },
            failures: 0,
        });
    }

    let avail_by_job: HashMap<&JobId, &AvailKey> = avail.iter().map(|a| (&a.job_id, a)).collect();
    let leases_by_job: HashMap<&JobId, Vec<&Lease>> =
        leases.iter().fold(HashMap::new(), |mut acc, lease| {
            acc.entry(&lease.job_id).or_default().push(lease);
            acc
        });

    let mut retired: HashSet<JobId> = HashSet::new();
    let mut released: HashMap<PlanId, HashSet<JobId>> = HashMap::new();
    let mut perturbed: HashSet<PlanId> = HashSet::new();
    let mut failures = 0usize;
    for (manifest, cause) in doomed {
        let mut jobs_retired = 0usize;
        let mut jobs_released = 0usize;
        let mut jobs_already_gone = 0usize;
        for job_id in &manifest.job_ids {
            // Released in an earlier pass: redoing it would only re-read the
            // tree every run for as long as the shared job lives.
            if membership.released_by(job_id, &manifest.plan_id) {
                continue;
            }
            let entry = avail_by_job.get(job_id).copied();
            let held = leases_by_job.get(job_id).map(Vec::as_slice).unwrap_or(&[]);
            if entry.is_none() && held.is_empty() {
                // Finished, retired by an earlier pass, or never promoted.
                continue;
            }
            match end_membership(todo, plans, &manifest.plan_id, job_id, entry, held).await {
                Ok(JobFate::Retired) => {
                    retired.insert(job_id.clone());
                    jobs_retired += 1;
                    tracing::debug!(
                        plan_id = %manifest.plan_id,
                        job_id = %job_id,
                        cause = cause.label(),
                        "queue-maintenance: retired a job of an ending plan"
                    );
                }
                Ok(JobFate::Released { surviving_plans }) => {
                    released
                        .entry(manifest.plan_id.clone())
                        .or_default()
                        .insert(job_id.clone());
                    jobs_released += 1;
                    tracing::info!(
                        plan_id = %manifest.plan_id,
                        job_id = %job_id,
                        surviving_plans,
                        cause = cause.label(),
                        "queue-maintenance: released a shared job to its other plans"
                    );
                }
                // Not retired by this call, so not perturbed; it still leaves
                // the plan's outstanding set. The age-gated GC takes its markers.
                Ok(JobFate::AlreadyGone) => {
                    released
                        .entry(manifest.plan_id.clone())
                        .or_default()
                        .insert(job_id.clone());
                    jobs_already_gone += 1;
                    tracing::info!(
                        plan_id = %manifest.plan_id,
                        job_id = %job_id,
                        cause = cause.label(),
                        "queue-maintenance: dropped the membership of a job that had already left the queue"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        plan_id = %manifest.plan_id,
                        job_id = %job_id,
                        error = %e,
                        "queue-maintenance: failed to retire a job of a cancelled plan"
                    );
                    failures += 1;
                }
            }
        }
        if jobs_retired > 0 {
            perturbed.insert(manifest.plan_id.clone());
        }
        if jobs_retired + jobs_released + jobs_already_gone > 0 {
            tracing::info!(
                plan_id = %manifest.plan_id,
                jobs_retired,
                jobs_released,
                jobs_already_gone,
                cause = cause.label(),
                "queue-maintenance: ended the live jobs of a plan"
            );
        }
    }

    let surviving: Vec<Lease> = leases
        .into_iter()
        .filter(|l| !retired.contains(&l.job_id))
        .collect();
    Ok(TeardownOutcome {
        avail: avail
            .into_iter()
            .filter(|a| !retired.contains(&a.job_id))
            .collect(),
        leased: surviving,
        retired,
        endings: PlanEndings {
            released,
            perturbed,
            stranded,
        },
        failures,
    })
}

/// What teardown did with one job of an ending plan.
enum JobFate {
    Retired,
    Released {
        surviving_plans: usize,
    },
    /// No other plan holds it, and it left the queue before the deletes landed:
    /// live under an unlisted lease, or already resolved.
    AlreadyGone,
}

/// Drop one plan's membership of one job, and retire the job when that was the
/// last membership.
///
/// Listings are fresh: a joiner writes its marker before re-checking liveness,
/// so a missed marker's joiner will see the retire and mint a new job. A joiner
/// that lands inside the retire window is caught by the re-list, and the job is
/// restored under its original key.
async fn end_membership(
    todo: &dyn TodoStore,
    plans: &dyn PlanStore,
    plan_id: &PlanId,
    job_id: &JobId,
    entry: Option<&AvailKey>,
    held: &[&Lease],
) -> anyhow::Result<JobFate> {
    // Fresh listing, so a marker the run's snapshot never saw is still dropped.
    for expires_at in plans
        .list_job_plan_markers(job_id)
        .await?
        .into_iter()
        .filter(|(member, _)| member == plan_id)
        .map(|(_, expires_at)| expires_at)
    {
        plans
            .delete_job_plan_marker(job_id, plan_id, expires_at)
            .await?;
    }

    let remaining = plans.list_job_plan_markers(job_id).await?;
    if !remaining.is_empty() {
        return Ok(JobFate::Released {
            surviving_plans: remaining.len(),
        });
    }

    // Read before the deletes so a restore can put the job back as it stood.
    let (expires_at, body) = match entry {
        Some(entry) => (
            entry.expires_at,
            todo.get_avail(job_id, entry.expires_at).await?,
        ),
        None => match held.first() {
            Some(lease) => (
                lease.expires_at,
                todo.get_leased(
                    job_id,
                    lease.expires_at,
                    &lease.client_id,
                    lease.lease_expiry,
                )
                .await?,
            ),
            // Unreachable through `tear_down_plans`; nothing is left to delete.
            None => return Ok(JobFate::Retired),
        },
    };

    // From here to the promote the body has no queue copy: a crash loses a job a
    // joiner holds (PIP-543). Not staging first: nothing promotes an orphan `tmp/`.
    retire_job(todo, job_id, entry.is_some(), held).await?;

    let joiners = plans.list_job_plan_markers(job_id).await?;
    // Usually claimed since the snapshot, so live under an unlisted lease: never
    // report it retired, or the orphan sweep strips its `eligible/` markers.
    let Some(body) = body else {
        // Flag it either way: the deletes above may have taken its `avail/`
        // entry, so its markers need re-matching when it returns.
        todo.write_pending_reindex_job(job_id).await?;
        if joiners.is_empty() {
            tracing::warn!(
                plan_id = %plan_id,
                job_id = %job_id,
                "queue-maintenance: a job left the queue before teardown could retire it"
            );
            return Ok(JobFate::AlreadyGone);
        }
        tracing::warn!(
            plan_id = %plan_id,
            job_id = %job_id,
            joining_plans = joiners.len(),
            "queue-maintenance: released a job whose body was already gone"
        );
        return Ok(JobFate::Released {
            surviving_plans: joiners.len(),
        });
    };
    if joiners.is_empty() {
        return Ok(JobFate::Retired);
    }
    todo.write_tmp(job_id, &body).await?;
    todo.promote_avail(job_id, expires_at).await?;
    // Out of `avail/` while the index ran, so its markers may be stale.
    todo.write_pending_reindex_job(job_id).await?;
    tracing::info!(
        plan_id = %plan_id,
        job_id = %job_id,
        joining_plans = joiners.len(),
        "queue-maintenance: restored a job a plan joined inside the retire window"
    );
    Ok(JobFate::Released {
        surviving_plans: joiners.len(),
    })
}

/// Why a plan owes teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Cause {
    Cancelled,
    CreatingStranded,
}

impl Cause {
    fn label(self) -> &'static str {
        self.into()
    }
}

fn teardown_cause(
    manifest: &PlanManifest,
    markers: &HashSet<PlanId>,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> Option<Cause> {
    match manifest.status {
        // `cancelled` is re-checked to catch a straggler, such as a lease a
        // heartbeat renamed under an earlier delete.
        PlanStatus::Cancelling | PlanStatus::Cancelled => Some(Cause::Cancelled),
        // A marker naming a `complete` plan is stray; reconciliation collects it.
        PlanStatus::Complete => None,
        // Teardown runs in the latching pass, not a cron interval later.
        // `creating` on purpose: a cancel may land mid-ingest.
        PlanStatus::Creating | PlanStatus::Active | PlanStatus::PendingClients
            if markers.contains(&manifest.plan_id) =>
        {
            Some(Cause::Cancelled)
        }
        // A future `created_at` fails `to_std` and reads as not stranded, the
        // safe direction.
        PlanStatus::Creating
            if now
                .signed_duration_since(manifest.created_at)
                .to_std()
                .is_ok_and(|age| age >= creating_max_age) =>
        {
            Some(Cause::CreatingStranded)
        }
        PlanStatus::Creating | PlanStatus::Active | PlanStatus::PendingClients => None,
    }
}

/// Every delete is idempotent, so an entry that vanishes mid-call is a no-op.
async fn retire_job(
    todo: &dyn TodoStore,
    job_id: &JobId,
    in_avail: bool,
    held: &[&Lease],
) -> anyhow::Result<()> {
    if in_avail {
        // By job id: a recycled or re-dated job can carry an `expires_at` this
        // run never listed.
        todo.delete_avail_by_job(job_id).await?;
    }
    for lease in held {
        todo.delete_lease(
            job_id,
            lease.expires_at,
            &lease.client_id,
            lease.lease_expiry,
        )
        .await?;
    }
    Ok(())
}

/// Record every member plan's lapse of a job whose horizon for that plan (the
/// latest of its markers) has passed. The lapse is final.
///
/// Manifest first, markers second: marker absence alone reads as a release, so
/// the reverse order loses the lapse on a crash. `manifests` is updated in place
/// because [`reconcile_plans`] rewrites from it and would clobber the lapse.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds"
)]
pub(crate) async fn record_member_lapses(
    plans: &dyn PlanStore,
    submissions: &dyn SubmissionStore,
    manifests: &mut [PlanManifest],
    markers: &[JobPlanMarker],
    cancel_markers: &HashSet<PlanId>,
    stranded: &HashSet<PlanId>,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> usize {
    let horizons: BTreeMap<(&PlanId, &JobId), ExpiresAt> =
        markers.iter().fold(BTreeMap::new(), |mut acc, marker| {
            acc.entry((&marker.plan_id, &marker.job_id))
                .and_modify(|latest| *latest = (*latest).max(marker.expires_at))
                .or_insert(marker.expires_at);
            acc
        });
    let slots: HashMap<PlanId, usize> = manifests
        .iter()
        .enumerate()
        .map(|(slot, manifest)| (manifest.plan_id.clone(), slot))
        .collect();

    let mut failures = 0usize;
    let mut recorded = 0usize;
    for ((plan_id, job_id), horizon) in horizons {
        let Some(deadline) = horizon.instant().filter(|_| horizon.is_expired(now)) else {
            continue;
        };
        // No manifest: the age-gated plan-index GC collects the marker.
        let Some(manifest) = slots.get(plan_id).and_then(|&slot| manifests.get_mut(slot)) else {
            continue;
        };
        // Teardown owns an ending plan's markers; a terminal latch is final.
        if manifest.status.is_terminal()
            || manifest.status == PlanStatus::Cancelling
            || cancel_markers.contains(plan_id)
            || stranded.contains(plan_id)
        {
            continue;
        }
        // An abandoned join: the plan never held the job. The GC owns the marker.
        if !manifest.job_ids.contains(job_id) {
            continue;
        }
        match submissions.has_record(job_id).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    plan_id = %plan_id,
                    job_id = %job_id,
                    error = %e,
                    "queue-maintenance: failed to check a lapsed job for a submission record"
                );
                failures += 1;
                continue;
            }
        }
        // Defer rather than write a record a staging ingest would overwrite.
        if ingest_may_overwrite(manifest, now, creating_max_age) {
            tracing::info!(
                plan_id = %plan_id,
                job_id = %job_id,
                horizon = %horizon,
                "queue-maintenance: deferred a lapse an ingest could still overwrite"
            );
            continue;
        }
        match record_one_lapse(plans, markers, plan_id, job_id, deadline, now).await {
            Ok(Some(updated)) => {
                *manifest = updated;
                recorded += 1;
            }
            // Deleted from storage by hand since the run's listing.
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    plan_id = %plan_id,
                    job_id = %job_id,
                    error = %e,
                    "queue-maintenance: failed to record a member plan's lapse of a job"
                );
                failures += 1;
            }
        }
    }
    if recorded > 0 {
        tracing::info!(recorded, "queue-maintenance: member-lapse pass done");
    }
    failures
}

/// Record one plan's lapse of one job, then drop that plan's markers on it.
///
/// Reads the manifest fresh: the run's listing predates the run's own writes,
/// and `put_plan` writes the whole record.
async fn record_one_lapse(
    plans: &dyn PlanStore,
    markers: &[JobPlanMarker],
    plan_id: &PlanId,
    job_id: &JobId,
    horizon: DateTime<Utc>,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<PlanManifest>> {
    let Some(mut manifest) = plans.get_plan(plan_id).await? else {
        return Ok(None);
    };
    if !manifest.expired_jobs.contains_key(job_id) {
        manifest.expired_jobs.insert(
            job_id.clone(),
            ExpiredJob {
                horizon,
                recorded_at: now,
            },
        );
        plans.put_plan(&manifest).await?;
        tracing::info!(
            plan_id = %plan_id,
            job_id = %job_id,
            horizon = %horizon,
            "queue-maintenance: recorded a member plan's lapse of a job"
        );
    }
    // Every marker goes: the latest of them is the horizon that just lapsed.
    for expires_at in markers
        .iter()
        .filter(|marker| &marker.plan_id == plan_id && &marker.job_id == job_id)
        .map(|marker| marker.expires_at)
    {
        plans
            .delete_job_plan_marker(job_id, plan_id, expires_at)
            .await?;
        tracing::info!(
            plan_id = %plan_id,
            job_id = %job_id,
            expires_at = %expires_at,
            "queue-maintenance: dropped the membership marker of a lapsed horizon"
        );
    }
    Ok(Some(manifest))
}

/// What [`reconcile_plans`] did, for the run's exit status and its summary.
#[derive(Debug, Default)]
pub(crate) struct ReconcileReport {
    pub(crate) failures: usize,
    /// Includes the probes of a plan whose reconciliation failed.
    pub(crate) probes: usize,
    pub(crate) reconciled: usize,
    pub(crate) count_jobs: Duration,
    pub(crate) starvation: Duration,
    pub(crate) put_plan: Duration,
    pub(crate) cancel_markers: Duration,
    pub(crate) slowest: Option<SlowestPlan>,
}

/// The plan that took [`reconcile_plans`] the most time.
#[derive(Debug)]
pub(crate) struct SlowestPlan {
    pub(crate) plan_id: PlanId,
    pub(crate) elapsed: Duration,
    pub(crate) probes: usize,
}

/// Write every plan's `status`, `progress_snapshot`, and `terminal_at`, and
/// collect the cancel markers that can no longer lead to work.
///
/// `workers` must be the fleet the claim path admits, so starvation says what
/// the claim door would do. Lenient per plan.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds"
)]
pub(crate) async fn reconcile_plans(
    plans: &dyn PlanStore,
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    manifests: &[PlanManifest],
    markers: &HashSet<PlanId>,
    endings: &PlanEndings,
    membership: &Membership,
    avail: &[AvailKey],
    leased_ids: &HashSet<JobId>,
    live_ids: &HashSet<JobId>,
    workers: &[&Client],
    tags: &FleetTags,
    bodies: &mut HashMap<String, Value>,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> anyhow::Result<ReconcileReport> {
    let avail_by_job: HashMap<&JobId, &AvailKey> = avail.iter().map(|a| (&a.job_id, a)).collect();

    let mut report = ReconcileReport::default();
    // Terminal plans included: a campaign revision carries its usually terminal
    // predecessor's finished jobs, and this spares re-reading their records.
    let mut settled: HashMap<JobId, SettledOutcome> = manifests
        .iter()
        .flat_map(|m| {
            m.settled_jobs
                .iter()
                .map(|(job_id, outcome)| (job_id.clone(), *outcome))
        })
        .collect();
    for manifest in manifests {
        let ending = endings.for_plan(&manifest.plan_id, markers.contains(&manifest.plan_id));
        let plan_started = Instant::now();
        let probes_before = report.probes;
        match reconcile_one_plan(
            plans,
            todo,
            submissions,
            manifest,
            &ending,
            membership,
            &avail_by_job,
            leased_ids,
            live_ids,
            workers,
            tags,
            bodies,
            &mut settled,
            &mut report,
            now,
            creating_max_age,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    plan_id = %manifest.plan_id,
                    elapsed_ms = millis(plan_started.elapsed()),
                    error = %e,
                    "queue-maintenance: failed to reconcile a plan"
                );
                report.failures += 1;
            }
        }
        let elapsed = plan_started.elapsed();
        if report
            .slowest
            .as_ref()
            .is_none_or(|slowest| elapsed > slowest.elapsed)
        {
            report.slowest = Some(SlowestPlan {
                plan_id: manifest.plan_id.clone(),
                elapsed,
                probes: report.probes - probes_before,
            });
        }
    }

    // A marker with no manifest would linger unseen, so collect it.
    let known: HashSet<&PlanId> = manifests.iter().map(|m| &m.plan_id).collect();
    for plan_id in markers.iter().filter(|id| !known.contains(id)) {
        let collected = timed(
            &mut report.cancel_markers,
            collect_orphaned_marker(plans, plan_id),
        )
        .await;
        if let Err(e) = collected {
            tracing::warn!(
                plan_id = %plan_id,
                error = %e,
                "queue-maintenance: failed to collect an orphaned cancel marker"
            );
            report.failures += 1;
        }
    }

    Ok(report)
}

/// Reconcile one plan: pick its status, refresh its snapshot, and collect its
/// cancel marker once the latch is in place.
#[expect(
    clippy::too_many_arguments,
    reason = "`reconcile_plans`'s context, one plan narrower"
)]
async fn reconcile_one_plan(
    plans: &dyn PlanStore,
    todo: &dyn TodoStore,
    submissions: &dyn SubmissionStore,
    manifest: &PlanManifest,
    ending: &Ending<'_>,
    membership: &Membership,
    avail_by_job: &HashMap<&JobId, &AvailKey>,
    leased_ids: &HashSet<JobId>,
    live_ids: &HashSet<JobId>,
    workers: &[&Client],
    tags: &FleetTags,
    bodies: &mut HashMap<String, Value>,
    settled: &mut HashMap<JobId, SettledOutcome>,
    report: &mut ReconcileReport,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> anyhow::Result<()> {
    // A terminal latch is final: a late marker is collected, `status` untouched.
    if manifest.status.is_terminal() {
        if ending.requested && !ingest_may_overwrite(manifest, now, creating_max_age) {
            timed(
                &mut report.cancel_markers,
                plans.delete_cancel_marker(&manifest.plan_id),
            )
            .await?;
            tracing::info!(
                plan_id = %manifest.plan_id,
                status = manifest.status.label(),
                "queue-maintenance: collected a cancel marker for an already-terminal plan"
            );
        }
        return Ok(());
    }

    let is_ending =
        ending.requested || ending.stranded || manifest.status == PlanStatus::Cancelling;

    // Ingestion owns a live `creating` manifest: nothing is promoted yet, so it
    // would read `complete`.
    if manifest.status == PlanStatus::Creating && !is_ending {
        return Ok(());
    }
    let started = Instant::now();
    let probes_before = report.probes;

    // The whole live set, not this pass's views: a lease recycled mid-run is in
    // neither listing, and reading it dead would latch `complete` for good.
    // Releases count for ending plans only; elsewhere a missing own marker beside
    // a co-owner's would read as a release and latch `complete` early.
    let outstanding: Vec<&JobId> = manifest
        .job_ids
        .iter()
        .filter(|job_id| live_ids.contains(*job_id))
        .filter(|job_id| !manifest.expired_jobs.contains_key(*job_id))
        .filter(|job_id| {
            !is_ending
                || !ending
                    .released_this_pass
                    .is_some_and(|jobs| jobs.contains(*job_id))
        })
        .filter(|job_id| !is_ending || !membership.released_by(job_id, &manifest.plan_id))
        .collect();

    // A leased job matched a client, so only `avail/` jobs can be starved.
    let (starved_ids, any_matched) = timed(&mut report.starvation, async {
        let mut starved_ids: Vec<&JobId> = Vec::new();
        let mut any_matched = false;
        for job_id in &outstanding {
            let Some(entry) = avail_by_job.get(*job_id) else {
                any_matched = true;
                continue;
            };
            let Some(body) = cached_body(todo, entry, bodies).await? else {
                // Left `avail/` since the listing, so a client took it.
                any_matched = true;
                continue;
            };
            if workers.iter().any(|c| job_matches_client(&body, c, tags)) {
                any_matched = true;
            } else {
                starved_ids.push(*job_id);
            }
        }
        anyhow::Ok((starved_ids, any_matched))
    })
    .await?;

    let (status, terminal_at) = decide_status(
        is_ending,
        outstanding.is_empty(),
        ending.retired_this_pass,
        any_matched,
        now,
    );

    let counted = timed(
        &mut report.count_jobs,
        count_jobs(
            submissions,
            manifest,
            avail_by_job,
            leased_ids,
            settled,
            &mut report.probes,
        ),
    )
    .await?;
    let snapshot = ProgressSnapshot {
        computed_at: now,
        counts: counted.counts,
        starved: timed(
            &mut report.starvation,
            group_starved(todo, &starved_ids, avail_by_job, bodies),
        )
        .await?,
    };

    let updated = PlanManifest {
        status,
        // Stamped once; the moment the plan ended never moves.
        terminal_at: manifest.terminal_at.or(terminal_at),
        progress_snapshot: Some(snapshot),
        settled_jobs: counted.settled_jobs,
        ..manifest.clone()
    };
    timed(&mut report.put_plan, plans.put_plan(&updated)).await?;
    report.reconciled += 1;
    // `DEBUG` when nothing changed, so a plan waiting on clients is quiet.
    let elapsed_ms = millis(started.elapsed());
    let probes = report.probes - probes_before;
    if probes > 0 || status != manifest.status {
        tracing::info!(
            plan_id = %manifest.plan_id,
            status = status.label(),
            probes,
            settled = counted.known,
            elapsed_ms,
            "queue-maintenance: refreshed a plan's progress snapshot"
        );
    } else {
        tracing::debug!(
            plan_id = %manifest.plan_id,
            status = status.label(),
            settled = counted.known,
            elapsed_ms,
            "queue-maintenance: refreshed a plan's progress snapshot with no probe \
             and no status change"
        );
    }
    if status != manifest.status {
        tracing::info!(
            plan_id = %manifest.plan_id,
            from = manifest.status.label(),
            to = status.label(),
            "queue-maintenance: plan status changed"
        );
    }

    if ending.requested {
        if ingest_may_overwrite(manifest, now, creating_max_age) {
            tracing::info!(
                plan_id = %manifest.plan_id,
                status = manifest.status.label(),
                "queue-maintenance: latched a cancellation while an ingest could still \
                 overwrite it; keeping the marker"
            );
        } else {
            timed(
                &mut report.cancel_markers,
                plans.delete_cancel_marker(&manifest.plan_id),
            )
            .await?;
            tracing::info!(
                plan_id = %manifest.plan_id,
                "queue-maintenance: latched the cancellation and collected its marker"
            );
        }
    }

    Ok(())
}

async fn timed<T>(slot: &mut Duration, work: impl std::future::Future<Output = T>) -> T {
    let started = Instant::now();
    let output = work.await;
    *slot += started.elapsed();
    output
}

/// Whether a staging ingest could still overwrite the status this pass writes
/// (`put_plan` keeps no latch), so the cancel marker must survive the run.
///
/// Only `creating` and the cancel latches can sit over a live ingest, and none
/// past `plan_creating_max_age_secs`. A future `created_at` reads as young.
fn ingest_may_overwrite(
    manifest: &PlanManifest,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> bool {
    let overwritable = matches!(
        manifest.status,
        PlanStatus::Creating | PlanStatus::Cancelling | PlanStatus::Cancelled
    );
    let age = now.signed_duration_since(manifest.created_at).to_std();
    overwritable && !matches!(age, Ok(age) if age >= creating_max_age)
}

/// Pick a non-terminal plan's status, and the `terminal_at` stamp a terminal one
/// carries.
///
/// Settles a cancellation only if this pass retired none of the plan's jobs: a
/// heartbeat can rename a lease so a reported delete was a no-op.
fn decide_status(
    ending: bool,
    outstanding_empty: bool,
    retired_this_pass: bool,
    any_matched: bool,
    now: DateTime<Utc>,
) -> (PlanStatus, Option<DateTime<Utc>>) {
    if ending && outstanding_empty && !retired_this_pass {
        (PlanStatus::Cancelled, Some(now))
    } else if ending {
        (PlanStatus::Cancelling, None)
    } else if outstanding_empty {
        (PlanStatus::Complete, Some(now))
    } else if any_matched {
        // Partial starvation stays `active`; the snapshot lists the starved.
        (PlanStatus::Active, None)
    } else {
        (PlanStatus::PendingClients, None)
    }
}

/// Delete a cancel marker whose plan is missing from the run's manifest listing,
/// after a fresh read confirms the manifest is really gone.
///
/// The listing spans real time, so a plan ingested during it is absent yet live.
async fn collect_orphaned_marker(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<()> {
    if plans.get_plan(plan_id).await?.is_some() {
        tracing::info!(
            plan_id = %plan_id,
            "queue-maintenance: a plan was ingested during this run's manifest listing; \
             keeping its cancel marker for the next run"
        );
        return Ok(());
    }
    plans.delete_cancel_marker(plan_id).await?;
    tracing::info!(
        plan_id = %plan_id,
        "queue-maintenance: collected a cancel marker for a plan with no manifest"
    );
    Ok(())
}

/// Count a plan's jobs into the six snapshot buckets.
///
/// Each record is probed once, then its outcome lives in `settled_jobs`: a run's
/// cost must not grow with finished jobs, or it delays the next lease recycle.
/// A lapsed job counts `failed` first, whatever the queue says.
async fn count_jobs(
    submissions: &dyn SubmissionStore,
    manifest: &PlanManifest,
    avail_by_job: &HashMap<&JobId, &AvailKey>,
    leased_ids: &HashSet<JobId>,
    settled: &mut HashMap<JobId, SettledOutcome>,
    probes: &mut usize,
) -> anyhow::Result<JobCount> {
    let mut counts = Counts {
        total: manifest.job_ids.len() as u64,
        finished: 0,
        running: 0,
        available: 0,
        failed: 0,
        cancelled: 0,
    };
    let mut settled_jobs = manifest.settled_jobs.clone();
    let mut known = 0usize;
    let mut to_probe: Vec<JobId> = Vec::new();
    for job_id in &manifest.job_ids {
        if manifest.expired_jobs.contains_key(job_id) {
            counts.failed += 1;
        } else if avail_by_job.contains_key(job_id) {
            counts.available += 1;
        } else if leased_ids.contains(job_id) {
            counts.running += 1;
        } else if let Some(&outcome) = manifest.settled_jobs.get(job_id) {
            add_outcome(&mut counts, outcome);
            known += 1;
        } else if let Some(&outcome) = settled.get(job_id) {
            add_outcome(&mut counts, outcome);
            settled_jobs.insert(job_id.clone(), outcome);
            known += 1;
        } else {
            to_probe.push(job_id.clone());
        }
    }
    // A record in `incoming/` or the score-queue counts: the client is done.
    *probes += to_probe.len();
    let mut found = submissions.find_outcomes(&to_probe).await?;
    to_probe
        .into_iter()
        .for_each(|job_id| match found.remove(&job_id) {
            Some(outcome) => {
                add_outcome(&mut counts, outcome);
                settled.insert(job_id.clone(), outcome);
                settled_jobs.insert(job_id, outcome);
            }
            // Retired by teardown; a record can still arrive, so not settled.
            None => counts.cancelled += 1,
        });
    Ok(JobCount {
        counts,
        settled_jobs,
        known,
    })
}

/// What [`count_jobs`] found for one plan.
struct JobCount {
    counts: Counts,
    settled_jobs: BTreeMap<JobId, SettledOutcome>,
    /// Jobs counted from a known outcome, with no probe.
    known: usize,
}

fn add_outcome(counts: &mut Counts, outcome: SettledOutcome) {
    match outcome {
        SettledOutcome::Finished => counts.finished += 1,
        SettledOutcome::Failed => counts.failed += 1,
    }
}

/// Group the starved jobs by requirement set, keyed like the manifest's
/// ingestion-time `warnings` so the two compare.
async fn group_starved(
    todo: &dyn TodoStore,
    starved_ids: &[&JobId],
    avail_by_job: &HashMap<&JobId, &AvailKey>,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<Vec<StarvedGroup>> {
    let mut groups: Vec<(crate::plan_ingestion::RequirementKey, Vec<JobId>)> = Vec::new();
    for job_id in starved_ids {
        let Some(entry) = avail_by_job.get(*job_id) else {
            continue;
        };
        let Some(body) = cached_body(todo, entry, bodies).await? else {
            continue;
        };
        let key = requirement_key(&body);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, ids)) => ids.push((*job_id).clone()),
            None => groups.push((key, vec![(*job_id).clone()])),
        }
    }
    Ok(groups
        .into_iter()
        .map(|(key, job_ids)| key.into_starved_group(job_ids))
        .collect())
}

/// Keeps plan-index candidates from colliding with `todo/` keys in the shared
/// `todo/.gc-candidates` set.
const PLAN_INDEX_KEY_PREFIX: &str = "plans:";

/// What [`gc_plan_index`] did, for the run's candidate set, exit status, and
/// summary.
pub(crate) struct PlanIndexGc {
    pub(crate) candidates: HashSet<String>,
    pub(crate) failures: usize,
    pub(crate) content_keys_read: usize,
}

/// Collect the orphans of the two plan-index keyspaces: content-key records
/// whose job is gone, and membership markers whose job or plan is gone.
///
/// Every arm is gated on `plan_creating_max_age_secs`: two-sighting alone is far
/// shorter than a slow ingest, whose markers it would collect. A marker whose
/// plan disowns a live job is collected too, or it pins a `never`-expiry job.
pub(crate) async fn gc_plan_index(
    plans: &dyn PlanStore,
    manifests: &[PlanManifest],
    markers: &[JobPlanMarker],
    live_ids: &HashSet<JobId>,
    prior: &HashSet<String>,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> PlanIndexGc {
    let plan_jobs: HashMap<&PlanId, HashSet<&JobId>> = manifests
        .iter()
        .map(|m| (&m.plan_id, m.job_ids.iter().collect()))
        .collect();
    let mut next = HashSet::new();
    let mut failures = 0usize;
    let mut records_removed = 0usize;
    let mut markers_removed = 0usize;
    let no_same_run_removals = HashSet::new();

    // A listing failure costs only the record arm; the marker arm still runs.
    let listed = match plans.list_content_keys().await {
        Ok(listed) => listed,
        Err(e) => {
            tracing::warn!(error = %e, "queue-maintenance: failed to list content keys");
            failures += 1;
            Vec::new()
        }
    };
    let content_keys_read = listed.len();
    for (digest, record, version) in listed {
        if live_ids.contains(&record.job_id) || !past_gate(record.created_at, now, creating_max_age)
        {
            continue;
        }
        let key = format!("{PLAN_INDEX_KEY_PREFIX}content-keys/v1/{digest}");
        if !confirm_or_stage(prior, &no_same_run_removals, &mut next, key, None) {
            continue;
        }
        match plans.delete_content_key(&digest, &version).await {
            // Replaced by another writer for its own job, so leaving it is right.
            Ok(pipette_store::ContentKeyCas::Lost) => tracing::info!(
                content_key = %digest,
                "queue-maintenance: content key was replaced while its orphan was collected"
            ),
            Ok(pipette_store::ContentKeyCas::Applied) => {
                records_removed += 1;
                tracing::info!(
                    content_key = %digest,
                    job_id = %record.job_id,
                    plan_id = %record.plan_id,
                    "queue-maintenance: collected the content key of a job that left the queue"
                );
            }
            Err(e) => {
                tracing::warn!(content_key = %digest, error = %e,
                    "queue-maintenance: failed to collect an orphaned content key");
                failures += 1;
            }
        }
    }

    for marker in markers {
        let job_dead = !live_ids.contains(&marker.job_id);
        let plan_disowns_job = plan_jobs
            .get(&marker.plan_id)
            .is_none_or(|held| !held.contains(&marker.job_id));
        if !(job_dead || plan_disowns_job) || !past_gate(marker.written_at, now, creating_max_age) {
            continue;
        }
        let key = format!(
            "{PLAN_INDEX_KEY_PREFIX}job-plans/{}/{}",
            marker.job_id,
            pipette_core::todo_filename::job_plan_marker_filename(
                &marker.plan_id,
                marker.expires_at
            )
        );
        if !confirm_or_stage(prior, &no_same_run_removals, &mut next, key, None) {
            continue;
        }
        match plans
            .delete_job_plan_marker(&marker.job_id, &marker.plan_id, marker.expires_at)
            .await
        {
            Ok(()) => {
                markers_removed += 1;
                tracing::info!(
                    job_id = %marker.job_id,
                    plan_id = %marker.plan_id,
                    "queue-maintenance: collected an orphaned membership marker"
                );
            }
            Err(e) => {
                tracing::warn!(
                    job_id = %marker.job_id,
                    plan_id = %marker.plan_id,
                    error = %e,
                    "queue-maintenance: failed to collect an orphaned membership marker"
                );
                failures += 1;
            }
        }
    }

    if records_removed + markers_removed > 0 {
        tracing::info!(
            records_removed,
            markers_removed,
            "queue-maintenance: collected orphaned plan-index keys"
        );
    }
    PlanIndexGc {
        candidates: next,
        failures,
        content_keys_read,
    }
}

/// Collect the pending source markers that outlived their ingest, with the
/// source a marker names when the ingest wrote no manifest.
///
/// Reads each manifest fresh: the run's snapshot omits an unparseable manifest,
/// which would read as absent and lose a live plan's source.
pub(crate) async fn gc_pending_plan_sources(
    plans: &dyn PlanStore,
    prior: &HashSet<String>,
    now: DateTime<Utc>,
    creating_max_age: Duration,
) -> (HashSet<String>, usize) {
    let mut next = HashSet::new();
    let markers = match plans.list_pending_source_markers().await {
        Ok(markers) => markers,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "queue-maintenance: failed to list pending source markers"
            );
            return (next, 1);
        }
    };
    let no_same_run_removals = HashSet::new();
    let mut failures = 0usize;
    for marker in markers {
        if !past_gate(marker.written_at, now, creating_max_age) {
            continue;
        }
        let key = format!(
            "{PLAN_INDEX_KEY_PREFIX}plan-sources-pending/{}",
            marker.plan_id
        );
        if !confirm_or_stage(prior, &no_same_run_removals, &mut next, key, None) {
            continue;
        }
        if let Err(e) = collect_pending_source(plans, &marker.plan_id).await {
            tracing::warn!(
                plan_id = %marker.plan_id,
                error = format!("{e:#}"),
                "queue-maintenance: failed to collect a pending plan source"
            );
            failures += 1;
        }
    }
    (next, failures)
}

async fn collect_pending_source(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<()> {
    let manifest = plans
        .get_plan(plan_id)
        .await
        .context("reading the manifest a pending source marker names")?;
    if manifest.is_none() {
        plans
            .delete_plan_source(plan_id)
            .await
            .context("deleting a plan source that has no manifest")?;
    }
    plans
        .delete_pending_source_marker(plan_id)
        .await
        .context("deleting a pending source marker")?;
    if manifest.is_none() {
        tracing::info!(
            plan_id = %plan_id,
            "queue-maintenance: collected the source of a plan that never wrote a manifest"
        );
    } else {
        tracing::info!(
            plan_id = %plan_id,
            "queue-maintenance: collected the pending marker of a stored plan source"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_store::{
        ContentKeyCas, ContentKeyClaim, ContentKeyRecord, ContentKeyVersion, CreateOutcome,
        JobPlanMarker, STORAGE_CONCURRENCY,
    };

    use super::*;

    /// Pinned here because [`reconcile_plans`] cannot inject the lease-rename
    /// race that makes a retiring pass hold `cancelling`.
    #[rstest]
    #[case::retired_holds_cancelling(true, true, true, false, PlanStatus::Cancelling, false)]
    #[case::settles_when_nothing_retired(true, true, false, false, PlanStatus::Cancelled, true)]
    #[case::outstanding_holds_cancelling(true, false, false, true, PlanStatus::Cancelling, false)]
    #[case::complete(false, true, false, false, PlanStatus::Complete, true)]
    #[case::active(false, false, false, true, PlanStatus::Active, false)]
    #[case::pending_clients(false, false, false, false, PlanStatus::PendingClients, false)]
    fn decide_status_rules(
        #[case] ending: bool,
        #[case] outstanding_empty: bool,
        #[case] retired_this_pass: bool,
        #[case] any_matched: bool,
        #[case] expected: PlanStatus,
        #[case] stamped: bool,
    ) -> anyhow::Result<()> {
        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let (status, terminal_at) = decide_status(
            ending,
            outstanding_empty,
            retired_this_pass,
            any_matched,
            now,
        );
        assert_eq!(status, expected);
        assert_eq!(terminal_at, stamped.then_some(now));
        Ok(())
    }

    /// A store whose content-key listing fails while its marker surface works.
    struct ListContentKeysFailsPlanStore {
        deleted: std::sync::Mutex<Vec<(JobId, PlanId)>>,
        unexpected: pipette_store_testkit::UnexpectedCalls,
    }

    #[async_trait::async_trait]
    impl PlanStore for ListContentKeysFailsPlanStore {
        async fn put_plan(&self, _manifest: &PlanManifest) -> anyhow::Result<()> {
            Err(self.unexpected.record("put_plan"))
        }
        async fn get_plan(&self, _plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
            Err(self.unexpected.record("get_plan"))
        }
        async fn list_matching_plans(
            &self,
            _filter: &pipette_store::PlanFilter,
            _after: Option<&PlanId>,
            _limit: std::num::NonZeroUsize,
        ) -> anyhow::Result<pipette_store::PlanPage> {
            Err(self.unexpected.record("list_matching_plans"))
        }
        async fn write_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_cancel_marker"))
        }
        async fn has_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<bool> {
            Err(self.unexpected.record("has_cancel_marker"))
        }
        async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
            Err(self.unexpected.record("list_cancel_markers"))
        }
        async fn delete_cancel_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_cancel_marker"))
        }
        async fn get_content_key(
            &self,
            _digest: &str,
        ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
            Err(self.unexpected.record("get_content_key"))
        }
        async fn create_content_key(
            &self,
            _digest: &str,
            _record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyClaim> {
            Err(self.unexpected.record("create_content_key"))
        }
        async fn replace_content_key(
            &self,
            _digest: &str,
            _version: &ContentKeyVersion,
            _record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyCas> {
            Err(self.unexpected.record("replace_content_key"))
        }
        async fn delete_content_key(
            &self,
            _digest: &str,
            _version: &ContentKeyVersion,
        ) -> anyhow::Result<ContentKeyCas> {
            Err(self.unexpected.record("delete_content_key"))
        }
        async fn list_content_keys(
            &self,
        ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
            anyhow::bail!("simulated listing failure")
        }
        async fn write_job_plan_marker(
            &self,
            _job_id: &JobId,
            _plan_id: &PlanId,
            _expires_at: pipette_plan_types::ExpiresAt,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_job_plan_marker"))
        }
        async fn list_job_plan_markers(
            &self,
            _job_id: &JobId,
        ) -> anyhow::Result<Vec<(PlanId, pipette_plan_types::ExpiresAt)>> {
            Err(self.unexpected.record("list_job_plan_markers"))
        }
        async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
            Err(self.unexpected.record("list_all_job_plan_markers"))
        }
        async fn delete_job_plan_marker(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
            _expires_at: pipette_plan_types::ExpiresAt,
        ) -> anyhow::Result<()> {
            self.deleted
                .lock()
                .map_err(|e| anyhow::anyhow!("poisoned: {e}"))?
                .push((job_id.clone(), plan_id.clone()));
            Ok(())
        }
        async fn put_plan_source(
            &self,
            _plan_id: &PlanId,
            _source: &pipette_core::plan::PlanSource,
        ) -> anyhow::Result<pipette_store::CreateOutcome> {
            Err(self.unexpected.record("put_plan_source"))
        }
        async fn get_plan_source(
            &self,
            _plan_id: &PlanId,
        ) -> anyhow::Result<Option<pipette_core::plan::PlanSource>> {
            Err(self.unexpected.record("get_plan_source"))
        }
        async fn delete_plan_source(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_plan_source"))
        }
        async fn write_pending_source_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_pending_source_marker"))
        }
        async fn list_pending_source_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_store::PendingSourceMarker>> {
            Err(self.unexpected.record("list_pending_source_markers"))
        }
        async fn delete_pending_source_marker(&self, _plan_id: &PlanId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_pending_source_marker"))
        }
        async fn put_job_source(
            &self,
            _job_id: &JobId,
            _body: &serde_json::Value,
        ) -> anyhow::Result<CreateOutcome> {
            Err(self.unexpected.record("put_job_source"))
        }
        async fn get_job_source(
            &self,
            _job_id: &JobId,
        ) -> anyhow::Result<Option<serde_json::Value>> {
            Err(self.unexpected.record("get_job_source"))
        }
        async fn put_job_plan_link(
            &self,
            _job_id: &JobId,
            _plan_id: &PlanId,
        ) -> anyhow::Result<CreateOutcome> {
            Err(self.unexpected.record("put_job_plan_link"))
        }
        async fn list_job_plan_links(&self, _job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
            Err(self.unexpected.record("list_job_plan_links"))
        }
    }

    #[tokio::test]
    async fn marker_arm_runs_when_the_content_key_listing_fails() -> anyhow::Result<()> {
        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let marker = JobPlanMarker {
            job_id: JobId::from_uuid(uuid::Uuid::from_u128(1)),
            plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(2)),
            expires_at: ExpiresAt::Never,
            written_at: now - chrono::Duration::hours(1),
        };
        // The prior run staged the marker's key, so this run may collect it.
        let prior = HashSet::from([format!(
            "{PLAN_INDEX_KEY_PREFIX}job-plans/{}/{}",
            marker.job_id,
            pipette_core::todo_filename::job_plan_marker_filename(
                &marker.plan_id,
                marker.expires_at
            )
        )]);
        let plans = ListContentKeysFailsPlanStore {
            deleted: std::sync::Mutex::new(Vec::new()),
            unexpected: pipette_store_testkit::UnexpectedCalls::default(),
        };

        let PlanIndexGc { failures, .. } = gc_plan_index(
            &plans,
            &[],
            std::slice::from_ref(&marker),
            &HashSet::new(),
            &prior,
            now,
            Duration::ZERO,
        )
        .await;

        assert_eq!(failures, 1, "the listing failure is reported");
        plans.unexpected.none()?;
        assert_eq!(
            *plans
                .deleted
                .lock()
                .map_err(|e| anyhow::anyhow!("poisoned: {e}"))?,
            vec![(marker.job_id.clone(), marker.plan_id.clone())],
            "the marker of a job that left the queue is collected anyway"
        );
        Ok(())
    }

    /// A [`PlanStore`] that lets a joining plan's marker appear at a chosen
    /// listing; the third lands it inside the retire window.
    struct JoinerAppearsPlanStore {
        inner: std::sync::Arc<dyn PlanStore>,
        listings: std::sync::atomic::AtomicUsize,
        inject_before: usize,
        job_id: JobId,
        joiner: PlanId,
    }

    #[async_trait::async_trait]
    impl PlanStore for JoinerAppearsPlanStore {
        async fn list_job_plan_markers(
            &self,
            job_id: &JobId,
        ) -> anyhow::Result<Vec<(PlanId, pipette_plan_types::ExpiresAt)>> {
            let nth = self
                .listings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if nth == self.inject_before {
                self.inner
                    .write_job_plan_marker(&self.job_id, &self.joiner, ExpiresAt::Never)
                    .await?;
            }
            self.inner.list_job_plan_markers(job_id).await
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
        async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
            self.inner.list_all_job_plan_markers().await
        }
        async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
            self.inner.put_plan(manifest).await
        }
        async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
            self.inner.get_plan(plan_id).await
        }
        async fn list_matching_plans(
            &self,
            filter: &pipette_store::PlanFilter,
            after: Option<&PlanId>,
            limit: std::num::NonZeroUsize,
        ) -> anyhow::Result<pipette_store::PlanPage> {
            self.inner.list_matching_plans(filter, after, limit).await
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
        ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
            self.inner.get_content_key(digest).await
        }
        async fn create_content_key(
            &self,
            digest: &str,
            record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyClaim> {
            self.inner.create_content_key(digest, record).await
        }
        async fn replace_content_key(
            &self,
            digest: &str,
            version: &ContentKeyVersion,
            record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyCas> {
            self.inner
                .replace_content_key(digest, version, record)
                .await
        }
        async fn delete_content_key(
            &self,
            digest: &str,
            version: &ContentKeyVersion,
        ) -> anyhow::Result<ContentKeyCas> {
            self.inner.delete_content_key(digest, version).await
        }
        async fn list_content_keys(
            &self,
        ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
            self.inner.list_content_keys().await
        }
        async fn put_plan_source(
            &self,
            plan_id: &PlanId,
            source: &pipette_core::plan::PlanSource,
        ) -> anyhow::Result<pipette_store::CreateOutcome> {
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
            body: &serde_json::Value,
        ) -> anyhow::Result<CreateOutcome> {
            self.inner.put_job_source(job_id, body).await
        }
        async fn get_job_source(
            &self,
            job_id: &JobId,
        ) -> anyhow::Result<Option<serde_json::Value>> {
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

    /// One teardown run over a job that stays live inside the retire window.
    struct RetireWindow {
        _dir: tempfile::TempDir,
        todo: std::sync::Arc<dyn TodoStore>,
        plans: std::sync::Arc<dyn PlanStore>,
        job_id: JobId,
        cancelling: PlanId,
        joiner: PlanId,
        body: Value,
        holder: Option<(pipette_plan_types::ClientId, DateTime<Utc>)>,
        outcome: TeardownOutcome,
    }

    async fn tear_down_in_retire_window(
        claim_first: bool,
        joiner_appears: bool,
    ) -> anyhow::Result<RetireWindow> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        })?;
        let todo = stores.todo.clone();

        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(7));
        let cancelling = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let joiner = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        let body = serde_json::json!({"job_id": job_id.as_str(), "requires": ["os:macos"]});
        todo.write_tmp(&job_id, &body).await?;
        todo.promote_avail(&job_id, ExpiresAt::Never).await?;
        stores
            .plans
            .write_job_plan_marker(&job_id, &cancelling, ExpiresAt::Never)
            .await?;

        // After the run's listings, so the snapshot below carries no lease.
        let holder = if claim_first {
            let client = pipette_plan_types::ClientId::try_new("holder")?;
            let lease_expiry: DateTime<Utc> = "2026-06-01T00:30:00Z".parse()?;
            anyhow::ensure!(
                matches!(
                    todo.claim_job(&job_id, ExpiresAt::Never, &client, lease_expiry)
                        .await?,
                    pipette_store::ClaimResult::Claimed(_)
                ),
                "the job is claimable before teardown runs"
            );
            Some((client, lease_expiry))
        } else {
            None
        };

        // The joiner's marker lands between the deletes and the re-listing.
        let plans: std::sync::Arc<dyn PlanStore> = if joiner_appears {
            std::sync::Arc::new(JoinerAppearsPlanStore {
                inner: stores.plans.clone(),
                listings: std::sync::atomic::AtomicUsize::new(0),
                inject_before: 3,
                job_id: job_id.clone(),
                joiner: joiner.clone(),
            })
        } else {
            stores.plans.clone()
        };

        let now = Utc::now();
        let manifest = PlanManifest {
            plan_id: cancelling.clone(),
            status: PlanStatus::Active,
            created_at: now,
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
            warnings: vec![],
            expired_jobs: BTreeMap::new(),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: None,
            terminal_at: None,
        };
        // The stale snapshot.
        let avail = vec![AvailKey {
            key: pipette_core::todo_filename::avail_filename(&job_id, ExpiresAt::Never),
            job_id: job_id.clone(),
            expires_at: ExpiresAt::Never,
        }];
        let membership = Membership::new(stores.plans.list_all_job_plan_markers().await?);

        let outcome = tear_down_plans(
            &*todo,
            &*plans,
            &[manifest],
            &HashSet::from([cancelling.clone()]),
            &membership,
            avail,
            Vec::new(),
            now,
            Duration::from_secs(21_600),
        )
        .await?;

        Ok(RetireWindow {
            _dir: dir,
            todo,
            plans: stores.plans.clone(),
            job_id,
            cancelling,
            joiner,
            body,
            holder,
            outcome,
        })
    }

    fn assert_not_retired(w: &RetireWindow) {
        assert!(
            w.outcome.retired.is_empty(),
            "the job is live, so the pass reports no retire"
        );
        assert_eq!(
            w.outcome.endings.released.get(&w.cancelling),
            Some(&HashSet::from([w.job_id.clone()])),
            "the job leaves the cancelling plan's outstanding set"
        );
        assert!(
            !w.outcome.endings.perturbed.contains(&w.cancelling),
            "the pass deleted nothing that survived, so it does not hold the settle"
        );
    }

    #[tokio::test]
    async fn a_job_joined_inside_the_retire_window_is_restored() -> anyhow::Result<()> {
        let w = tear_down_in_retire_window(false, true).await?;
        assert_not_retired(&w);

        let restored = w
            .todo
            .get_avail(&w.job_id, ExpiresAt::Never)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the job is back in avail/"))?;
        assert_eq!(
            restored, w.body,
            "restored under its original key, body intact"
        );
        assert_eq!(
            w.todo.list_pending_reindex_jobs().await?,
            vec![w.job_id.clone()],
            "the job was invisible to the index while it was gone, so it is re-matched"
        );
        assert_eq!(
            w.plans.list_job_plan_markers(&w.job_id).await?,
            vec![(w.joiner.clone(), ExpiresAt::Never)],
            "the joiner keeps its membership; the cancelling plan's is gone"
        );
        Ok(())
    }

    /// A `Retired` report would let the orphan sweep strip the live job's
    /// `eligible/` markers.
    #[tokio::test]
    async fn a_job_claimed_inside_the_retire_window_is_released() -> anyhow::Result<()> {
        let w = tear_down_in_retire_window(true, true).await?;
        assert_not_retired(&w);

        let (client, lease_expiry) = w
            .holder
            .clone()
            .ok_or_else(|| anyhow::anyhow!("this case claims the job"))?;
        assert!(
            w.todo
                .get_leased(&w.job_id, ExpiresAt::Never, &client, lease_expiry)
                .await?
                .is_some(),
            "the holder's lease survives teardown"
        );
        assert_eq!(
            w.todo.list_pending_reindex_jobs().await?,
            vec![w.job_id.clone()],
            "the job is flagged, so its markers are re-matched once it is queued again"
        );
        assert_eq!(
            w.plans.list_job_plan_markers(&w.job_id).await?,
            vec![(w.joiner.clone(), ExpiresAt::Never)],
            "the joiner keeps its membership; the cancelling plan's is gone"
        );
        Ok(())
    }

    /// The same claim with the cancelling plan as the only member: teardown
    /// reports it already gone.
    #[tokio::test]
    async fn a_claimed_job_with_no_other_member_is_not_reported_retired() -> anyhow::Result<()> {
        let w = tear_down_in_retire_window(true, false).await?;
        assert_not_retired(&w);

        let (client, lease_expiry) = w
            .holder
            .clone()
            .ok_or_else(|| anyhow::anyhow!("this case claims the job"))?;
        assert!(
            w.todo
                .get_leased(&w.job_id, ExpiresAt::Never, &client, lease_expiry)
                .await?
                .is_some(),
            "the holder's lease survives teardown"
        );
        assert_eq!(
            w.todo.list_pending_reindex_jobs().await?,
            vec![w.job_id.clone()],
            "the job is flagged, so its markers are re-matched once it is queued again"
        );
        assert!(
            w.plans.list_job_plan_markers(&w.job_id).await?.is_empty(),
            "the cancelling plan's membership is still dropped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_release_from_an_earlier_pass_is_not_repeated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        })?;
        let todo = stores.todo.clone();

        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(13));
        let settled = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let holder = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        let body = serde_json::json!({"job_id": job_id.as_str(), "requires": ["os:macos"]});
        todo.write_tmp(&job_id, &body).await?;
        todo.promote_avail(&job_id, ExpiresAt::Never).await?;
        // An earlier pass dropped this plan's marker; the other plan keeps its.
        stores
            .plans
            .write_job_plan_marker(&job_id, &holder, ExpiresAt::Never)
            .await?;

        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let manifest = PlanManifest {
            plan_id: settled.clone(),
            status: PlanStatus::Cancelled,
            created_at: now,
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
            warnings: vec![],
            expired_jobs: BTreeMap::new(),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: None,
            terminal_at: Some(now),
        };
        let avail = vec![AvailKey {
            key: pipette_core::todo_filename::avail_filename(&job_id, ExpiresAt::Never),
            job_id: job_id.clone(),
            expires_at: ExpiresAt::Never,
        }];
        let membership = Membership::new(stores.plans.list_all_job_plan_markers().await?);

        let outcome = tear_down_plans(
            &*todo,
            stores.plans.as_ref(),
            &[manifest],
            &HashSet::new(),
            &membership,
            avail,
            Vec::new(),
            now,
            Duration::from_secs(21_600),
        )
        .await?;

        assert!(
            outcome.endings.released.is_empty(),
            "a release performed runs ago is neither performed nor reported again"
        );
        assert!(outcome.retired.is_empty());
        assert!(!outcome.endings.perturbed.contains(&settled));
        assert_eq!(
            stores.plans.list_job_plan_markers(&job_id).await?,
            vec![(holder, ExpiresAt::Never)],
            "the holding plan's membership is untouched"
        );
        assert!(
            todo.get_avail_by_job(&job_id).await?.is_some(),
            "the shared job stays claimable"
        );
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum PendingManifest {
        Absent,
        Readable,
        Unreadable,
    }

    #[rstest]
    #[case::young(PendingManifest::Absent, false, true, true, true, false, 0)]
    #[case::first_sighting(PendingManifest::Absent, true, false, true, true, true, 0)]
    #[case::orphaned_source(PendingManifest::Absent, true, true, false, false, false, 0)]
    #[case::stored_plan(PendingManifest::Readable, true, true, true, false, false, 0)]
    #[case::unreadable_manifest(PendingManifest::Unreadable, true, true, true, true, false, 1)]
    #[tokio::test]
    async fn pending_source_sweep(
        #[case] manifest: PendingManifest,
        #[case] old: bool,
        #[case] seen_last_run: bool,
        #[case] source_stays: bool,
        #[case] marker_stays: bool,
        #[case] staged: bool,
        #[case] expected_failures: usize,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let plans = pipette_store_fs::build_local_fs_plan_store(
            &pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
        )?;
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let source = pipette_core::plan::PlanSource::new("benchmarks = [\"b\"]\n".to_string())?;
        plans.write_pending_source_marker(&plan_id).await?;
        assert_eq!(
            plans.put_plan_source(&plan_id, &source).await?,
            pipette_store::CreateOutcome::Created
        );
        match manifest {
            PendingManifest::Absent => {}
            PendingManifest::Readable => {
                plans
                    .put_plan(&pipette_core::plan::sample_manifest(
                        plan_id.clone(),
                        PlanStatus::Active,
                        false,
                    )?)
                    .await?;
            }
            PendingManifest::Unreadable => std::fs::write(
                dir.path().join("plans").join(format!("{plan_id}.json")),
                b"not a manifest",
            )?,
        }

        let creating_max_age = Duration::from_secs(21_600);
        let now = Utc::now() + chrono::Duration::hours(if old { 7 } else { 0 });
        let key = format!("{PLAN_INDEX_KEY_PREFIX}plan-sources-pending/{plan_id}");
        let prior: HashSet<String> = seen_last_run.then(|| key.clone()).into_iter().collect();

        let (next, failures) =
            gc_pending_plan_sources(plans.as_ref(), &prior, now, creating_max_age).await;

        assert_eq!(failures, expected_failures);
        assert_eq!(next.contains(&key), staged, "staged: {next:?}");
        assert_eq!(
            plans.get_plan_source(&plan_id).await?.is_some(),
            source_stays
        );
        assert_eq!(
            !plans.list_pending_source_markers().await?.is_empty(),
            marker_stays
        );
        Ok(())
    }

    // ── the lapse pass's crash ordering ────────────────────────────────────

    /// The step of the lapse pass a [`FailsOnePlanStore`] cuts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LapseStep {
        Record,
        MarkerDrop,
    }

    /// A [`PlanStore`] that fails one step of the lapse pass and delegates
    /// everything else to a real store.
    struct FailsOnePlanStore {
        inner: std::sync::Arc<dyn PlanStore>,
        fail: LapseStep,
    }

    #[async_trait::async_trait]
    impl PlanStore for FailsOnePlanStore {
        async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
            match self.fail {
                LapseStep::Record => anyhow::bail!("simulated manifest write failure"),
                LapseStep::MarkerDrop => self.inner.put_plan(manifest).await,
            }
        }
        async fn delete_job_plan_marker(
            &self,
            job_id: &JobId,
            plan_id: &PlanId,
            expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            match self.fail {
                LapseStep::MarkerDrop => anyhow::bail!("simulated marker delete failure"),
                LapseStep::Record => {
                    self.inner
                        .delete_job_plan_marker(job_id, plan_id, expires_at)
                        .await
                }
            }
        }
        async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
            self.inner.get_plan(plan_id).await
        }
        async fn list_matching_plans(
            &self,
            filter: &pipette_store::PlanFilter,
            after: Option<&PlanId>,
            limit: std::num::NonZeroUsize,
        ) -> anyhow::Result<pipette_store::PlanPage> {
            self.inner.list_matching_plans(filter, after, limit).await
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
        ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
            self.inner.get_content_key(digest).await
        }
        async fn create_content_key(
            &self,
            digest: &str,
            record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyClaim> {
            self.inner.create_content_key(digest, record).await
        }
        async fn replace_content_key(
            &self,
            digest: &str,
            version: &ContentKeyVersion,
            record: &ContentKeyRecord,
        ) -> anyhow::Result<ContentKeyCas> {
            self.inner
                .replace_content_key(digest, version, record)
                .await
        }
        async fn delete_content_key(
            &self,
            digest: &str,
            version: &ContentKeyVersion,
        ) -> anyhow::Result<ContentKeyCas> {
            self.inner.delete_content_key(digest, version).await
        }
        async fn list_content_keys(
            &self,
        ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
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
        async fn put_plan_source(
            &self,
            plan_id: &PlanId,
            source: &pipette_core::plan::PlanSource,
        ) -> anyhow::Result<pipette_store::CreateOutcome> {
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
            body: &serde_json::Value,
        ) -> anyhow::Result<CreateOutcome> {
            self.inner.put_job_source(job_id, body).await
        }
        async fn get_job_source(
            &self,
            job_id: &JobId,
        ) -> anyhow::Result<Option<serde_json::Value>> {
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

    /// A rerun after a failed marker drop keeps the first `recorded_at`.
    #[rstest]
    #[case::record_fails(LapseStep::Record)]
    #[case::marker_drop_fails(LapseStep::MarkerDrop)]
    #[tokio::test]
    async fn a_lapse_survives_a_death_at_either_side_of_its_record(
        #[case] fail: LapseStep,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        })?;
        let now: DateTime<Utc> = "2026-06-01T00:00:00Z".parse()?;
        let horizon = ExpiresAt::At("2026-05-01T00:00:00Z".parse::<DateTime<Utc>>()?);
        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(7));
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let manifest = PlanManifest {
            plan_id: plan_id.clone(),
            status: PlanStatus::Active,
            // Past the ingest window, so the overwrite rule does not defer.
            created_at: now - chrono::Duration::days(1),
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
            warnings: vec![],
            expired_jobs: BTreeMap::new(),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: None,
            terminal_at: None,
        };
        stores.plans.put_plan(&manifest).await?;
        stores
            .plans
            .write_job_plan_marker(&job_id, &plan_id, horizon)
            .await?;
        let markers = stores.plans.list_all_job_plan_markers().await?;

        let cut = FailsOnePlanStore {
            inner: stores.plans.clone(),
            fail,
        };
        let mut cut_manifests = vec![manifest.clone()];
        let failures = record_member_lapses(
            &cut,
            stores.submissions.as_ref(),
            &mut cut_manifests,
            &markers,
            &HashSet::new(),
            &HashSet::new(),
            now,
            Duration::from_secs(21_600),
        )
        .await;
        assert_eq!(failures, 1, "the cut step is reported");

        let after_cut = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("manifest"))?;
        match fail {
            LapseStep::Record => {
                assert!(after_cut.expired_jobs.is_empty(), "nothing was recorded");
                assert_eq!(
                    stores.plans.list_job_plan_markers(&job_id).await?,
                    vec![(plan_id.clone(), horizon)],
                    "the markers stand, so the next run re-detects the lapse"
                );
            }
            LapseStep::MarkerDrop => {
                assert!(
                    after_cut.expired_jobs.contains_key(&job_id),
                    "the record landed before the drop was attempted"
                );
            }
        }

        // The next run, against a healthy store, converges either way.
        let mut manifests = vec![after_cut];
        let failures = record_member_lapses(
            stores.plans.as_ref(),
            stores.submissions.as_ref(),
            &mut manifests,
            &markers,
            &HashSet::new(),
            &HashSet::new(),
            now + chrono::Duration::hours(1),
            Duration::from_secs(21_600),
        )
        .await;
        assert_eq!(failures, 0);

        let settled = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("manifest"))?;
        let entry = settled
            .expired_jobs
            .get(&job_id)
            .ok_or_else(|| anyhow::anyhow!("lapse recorded"))?;
        assert_eq!(ExpiresAt::At(entry.horizon), horizon);
        assert!(
            stores
                .plans
                .list_job_plan_markers(&job_id)
                .await?
                .is_empty(),
            "the markers are gone once the record is durable"
        );
        if fail == LapseStep::MarkerDrop {
            assert_eq!(
                entry.recorded_at, now,
                "a re-run keeps the moment the lapse was first detected"
            );
        }
        // The in-memory manifest carries the lapse, so reconcile won't clobber it.
        assert_eq!(manifests[0].expired_jobs, settled.expired_jobs);
        Ok(())
    }

    /// A job's state when `count_jobs` counts it; the string is the record's
    /// `message_type`.
    #[derive(Clone, Copy)]
    enum Seen {
        Avail,
        Leased,
        Known,
        Incoming(&'static str),
        Processed(&'static str),
        NoRecord,
    }

    /// Cases: (state, lapsed) per job, counts as `finished`, `running`,
    /// `available`, `failed`, `cancelled`, then probes. One `find_outcomes` call.
    #[rstest]
    #[case::avail(&[(Seen::Avail, false)], (0, 0, 1, 0, 0), 0)]
    #[case::leased(&[(Seen::Leased, false)], (0, 1, 0, 0, 0), 0)]
    #[case::known(&[(Seen::Known, false)], (1, 0, 0, 0, 0), 0)]
    #[case::incoming_success(&[(Seen::Incoming("success"), false)], (1, 0, 0, 0, 0), 1)]
    #[case::processed_failure(&[(Seen::Processed("failure"), false)], (0, 0, 0, 1, 0), 1)]
    #[case::no_record(&[(Seen::NoRecord, false)], (0, 0, 0, 0, 1), 1)]
    #[case::lapsed_beats_avail(&[(Seen::Avail, true)], (0, 0, 0, 1, 0), 0)]
    #[case::lapsed_beats_leased(&[(Seen::Leased, true)], (0, 0, 0, 1, 0), 0)]
    #[case::lapsed_beats_a_success(&[(Seen::Incoming("success"), true)], (0, 0, 0, 1, 0), 0)]
    #[case::many_probes(
        &[
            (Seen::Incoming("success"), false),
            (Seen::Processed("success"), false),
            (Seen::Processed("failure"), false),
            (Seen::NoRecord, false),
            (Seen::Incoming("failure"), false),
            (Seen::Avail, false),
            (Seen::Leased, false),
            (Seen::NoRecord, true),
            (Seen::Processed("success"), false),
            (Seen::Incoming("success"), false),
        ],
        (4, 1, 1, 3, 1),
        7,
    )]
    #[case::more_probes_than_slots(
        &[
            (Seen::Incoming("success"), false),
            (Seen::Processed("failure"), false),
            (Seen::NoRecord, false),
        ]
        .repeat(STORAGE_CONCURRENCY / 2),
        (8, 0, 0, 8, 8),
        24,
    )]
    #[tokio::test]
    async fn count_jobs_buckets(
        #[case] jobs: &[(Seen, bool)],
        #[case] (finished, running, available, failed, cancelled): (u64, u64, u64, u64, u64),
        #[case] probes: usize,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let submissions = RecordingOutcomes {
            inner: pipette_store_fs::LocalFsSubmissionStore::new(dir.path().to_path_buf()),
            calls: std::sync::Mutex::new(Vec::new()),
            unexpected: pipette_store_testkit::UnexpectedCalls::default(),
        };
        let ids: Vec<JobId> = (0..jobs.len())
            .map(|n| JobId::from_uuid(uuid::Uuid::from_u128(n as u128 + 1)))
            .collect();
        let mut avail = Vec::new();
        let mut leased_ids = HashSet::new();
        let mut expired_jobs = BTreeMap::new();
        let mut settled_jobs = BTreeMap::new();
        let mut to_probe = Vec::new();
        for (job_id, &(seen, lapsed)) in ids.iter().zip(jobs) {
            let record = |message_type: &str| serde_json::json!({"job_id": job_id.as_str(), "message_type": message_type});
            match seen {
                Seen::Avail => avail.push(AvailKey {
                    key: format!("{job_id}/never.json"),
                    job_id: job_id.clone(),
                    expires_at: ExpiresAt::Never,
                }),
                Seen::Leased => {
                    leased_ids.insert(job_id.clone());
                }
                Seen::Known => {
                    settled_jobs.insert(job_id.clone(), SettledOutcome::Finished);
                }
                Seen::Incoming(message_type) => {
                    submissions
                        .inner
                        .write_incoming(job_id, &record(message_type))
                        .await?;
                }
                Seen::Processed(message_type) => {
                    submissions
                        .inner
                        .write_incoming(job_id, &record(message_type))
                        .await?;
                    submissions.inner.mark_processed(job_id).await?;
                }
                Seen::NoRecord => {}
            }
            if !lapsed
                && matches!(
                    seen,
                    Seen::Incoming(_) | Seen::Processed(_) | Seen::NoRecord
                )
            {
                to_probe.push(job_id.clone());
            }
            if lapsed {
                expired_jobs.insert(
                    job_id.clone(),
                    ExpiredJob {
                        horizon: Utc::now() - chrono::Duration::hours(1),
                        recorded_at: Utc::now(),
                    },
                );
            }
        }
        let manifest = PlanManifest {
            job_ids: ids.clone(),
            expired_jobs,
            settled_jobs,
            ..pipette_core::plan::sample_manifest(
                PlanId::from_uuid(uuid::Uuid::nil()),
                PlanStatus::Active,
                false,
            )?
        };
        let avail_by_job: HashMap<&JobId, &AvailKey> =
            avail.iter().map(|a| (&a.job_id, a)).collect();

        let mut probed = 0;
        let counted = count_jobs(
            &submissions,
            &manifest,
            &avail_by_job,
            &leased_ids,
            &mut HashMap::new(),
            &mut probed,
        )
        .await?;

        assert_eq!(
            counted.counts,
            Counts {
                total: jobs.len() as u64,
                finished,
                running,
                available,
                failed,
                cancelled,
            }
        );
        assert_eq!(
            probed, probes,
            "only the jobs that left the queue are probed"
        );
        let calls = submissions
            .calls
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls, vec![to_probe]);
        submissions.unexpected.none()
    }

    /// Answers `find_outcomes` only; any other call, such as a whole-record read,
    /// fails the test.
    struct RecordingOutcomes {
        inner: pipette_store_fs::LocalFsSubmissionStore,
        calls: std::sync::Mutex<Vec<Vec<JobId>>>,
        unexpected: pipette_store_testkit::UnexpectedCalls,
    }

    #[async_trait::async_trait]
    impl SubmissionStore for RecordingOutcomes {
        async fn find_outcomes(
            &self,
            job_ids: &[JobId],
        ) -> anyhow::Result<HashMap<JobId, SettledOutcome>> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(job_ids.to_vec());
            self.inner.find_outcomes(job_ids).await
        }

        async fn write_incoming(&self, _: &JobId, _: &Value) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_incoming"))
        }
        async fn write_incoming_new(&self, _: &JobId, _: &Value) -> anyhow::Result<CreateOutcome> {
            Err(self.unexpected.record("write_incoming_new"))
        }
        async fn write_processed(&self, _: &JobId, _: &Value) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_processed"))
        }
        async fn get_submission(
            &self,
            _: &JobId,
        ) -> anyhow::Result<Option<pipette_store::SubmissionRecord>> {
            Err(self.unexpected.record("get_submission"))
        }
        async fn list_incoming_after(
            &self,
            _: Option<&JobId>,
            _: std::num::NonZeroUsize,
        ) -> anyhow::Result<Vec<JobId>> {
            Err(self.unexpected.record("list_incoming_after"))
        }
        async fn mark_processed(&self, _: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("mark_processed"))
        }
        async fn delete_incoming(&self, _: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_incoming"))
        }
        async fn find_job(
            &self,
            _: &JobId,
        ) -> anyhow::Result<Option<pipette_store::SubmissionRecord>> {
            Err(self.unexpected.record("find_job"))
        }
        async fn has_record(&self, _: &JobId) -> anyhow::Result<bool> {
            Err(self.unexpected.record("has_record"))
        }
        async fn find_in_score_queue(
            &self,
            _: &JobId,
        ) -> anyhow::Result<Option<pipette_store::SubmissionRecord>> {
            Err(self.unexpected.record("find_in_score_queue"))
        }
        async fn enqueue(
            &self,
            _: pipette_store::ScoreQueueStage,
            _: &JobId,
            _: &Value,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("enqueue"))
        }
        async fn list_queue_after(
            &self,
            _: pipette_store::ScoreQueueStage,
            _: Option<&JobId>,
            _: std::num::NonZeroUsize,
        ) -> anyhow::Result<Vec<JobId>> {
            Err(self.unexpected.record("list_queue_after"))
        }
        async fn read_queue(
            &self,
            _: pipette_store::ScoreQueueStage,
            _: &JobId,
        ) -> anyhow::Result<Option<Value>> {
            Err(self.unexpected.record("read_queue"))
        }
        async fn dequeue(
            &self,
            _: pipette_store::ScoreQueueStage,
            _: &JobId,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("dequeue"))
        }
        async fn write_unverified(
            &self,
            _: &pipette_plan_types::ClientId,
            _: &JobId,
            _: &Value,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_unverified"))
        }
        async fn list_unverified_client(
            &self,
            _: &pipette_plan_types::ClientId,
        ) -> anyhow::Result<Vec<(JobId, Value)>> {
            Err(self.unexpected.record("list_unverified_client"))
        }
        async fn delete_unverified(
            &self,
            _: &pipette_plan_types::ClientId,
            _: &JobId,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_unverified"))
        }
        async fn delete_unverified_client(
            &self,
            _: &pipette_plan_types::ClientId,
            _: bool,
        ) -> anyhow::Result<usize> {
            Err(self.unexpected.record("delete_unverified_client"))
        }
        async fn prune_unverified(
            &self,
            _: std::time::Duration,
            _: bool,
        ) -> anyhow::Result<pipette_store::PruneSummary> {
            Err(self.unexpected.record("prune_unverified"))
        }
        async fn write_runtime_output(
            &self,
            _: &JobId,
            _: &pipette_plan_types::ClientId,
            _: &pipette_plan_types::RuntimeOutput,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_runtime_output"))
        }
        async fn list_runtime_output(
            &self,
            _: &JobId,
        ) -> anyhow::Result<
            Vec<(
                pipette_plan_types::ClientId,
                pipette_plan_types::RuntimeOutput,
            )>,
        > {
            Err(self.unexpected.record("list_runtime_output"))
        }
    }

    /// A recordless job is read every run; the queue and `expired_jobs` beat a
    /// settled outcome.
    #[tokio::test]
    async fn settled_outcomes_are_not_read_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let submissions = pipette_store_fs::LocalFsSubmissionStore::new(dir.path().join("a"));
        // No records, so a count that reads one counts the job `cancelled`.
        let empty = pipette_store_fs::LocalFsSubmissionStore::new(dir.path().join("b"));
        let job = |n: u128| JobId::from_uuid(uuid::Uuid::from_u128(n));
        let (finished, failed, recordless) = (job(1), job(2), job(3));
        let record = |job_id: &JobId, message_type: &str| serde_json::json!({"job_id": job_id.as_str(), "message_type": message_type});
        submissions
            .write_incoming(&finished, &record(&finished, "success"))
            .await?;
        submissions
            .write_incoming(&failed, &record(&failed, "failure"))
            .await?;
        submissions.mark_processed(&failed).await?;
        let manifest = PlanManifest {
            job_ids: vec![finished.clone(), failed.clone(), recordless.clone()],
            expired_jobs: BTreeMap::new(),
            settled_jobs: BTreeMap::new(),
            ..pipette_core::plan::sample_manifest(
                PlanId::from_uuid(uuid::Uuid::nil()),
                PlanStatus::Active,
                false,
            )?
        };
        let counts = |finished, available, failed, cancelled| Counts {
            total: 3,
            finished,
            running: 0,
            available,
            failed,
            cancelled,
        };
        let no_avail = HashMap::new();
        let no_leases = HashSet::new();

        // The first run reads all three records and records two outcomes.
        let mut run_settled = HashMap::new();
        let mut probes = 0;
        let first = count_jobs(
            &submissions,
            &manifest,
            &no_avail,
            &no_leases,
            &mut run_settled,
            &mut probes,
        )
        .await?;
        assert_eq!(first.counts, counts(1, 0, 1, 1));
        assert_eq!((probes, first.known), (3, 0));
        let recorded = BTreeMap::from([
            (finished.clone(), SettledOutcome::Finished),
            (failed.clone(), SettledOutcome::Failed),
        ]);
        assert_eq!(first.settled_jobs, recorded);

        // Another plan in the same run takes the outcome from the run's map.
        let sharer = PlanManifest {
            job_ids: vec![finished.clone()],
            ..manifest.clone()
        };
        let mut probes = 0;
        let shared = count_jobs(
            &empty,
            &sharer,
            &no_avail,
            &no_leases,
            &mut run_settled,
            &mut probes,
        )
        .await?;
        assert_eq!(
            (probes, shared.known),
            (0, 1),
            "the run already read the shared job"
        );
        assert_eq!(shared.counts.finished, 1);
        assert_eq!(
            shared.settled_jobs,
            BTreeMap::from([(finished.clone(), SettledOutcome::Finished)]),
            "the sharing plan records the outcome for its own later runs"
        );

        // A later run reads only the job with no record.
        let settled = PlanManifest {
            settled_jobs: first.settled_jobs,
            ..manifest
        };
        let mut probes = 0;
        let later = count_jobs(
            &empty,
            &settled,
            &no_avail,
            &no_leases,
            &mut HashMap::new(),
            &mut probes,
        )
        .await?;
        assert_eq!(later.counts, counts(1, 0, 1, 1));
        assert_eq!(
            (probes, later.known),
            (1, 2),
            "only the job with no record is read again"
        );
        assert_eq!(later.settled_jobs, recorded);

        // A settled job back in `avail/` counts `available`, and a lapse counts
        // `failed` whatever the record said.
        let entry = AvailKey {
            key: format!("{finished}/never.json"),
            job_id: finished.clone(),
            expires_at: ExpiresAt::Never,
        };
        let avail = HashMap::from([(&finished, &entry)]);
        let lapsed = PlanManifest {
            expired_jobs: BTreeMap::from([(
                failed.clone(),
                ExpiredJob {
                    horizon: Utc::now() - chrono::Duration::hours(1),
                    recorded_at: Utc::now(),
                },
            )]),
            ..settled
        };
        let mut probes = 0;
        let requeued = count_jobs(
            &empty,
            &lapsed,
            &avail,
            &no_leases,
            &mut HashMap::new(),
            &mut probes,
        )
        .await?;
        assert_eq!(requeued.counts, counts(0, 1, 1, 1));
        assert_eq!(
            (probes, requeued.known),
            (1, 0),
            "the queue and a lapse decide ahead of a settled outcome"
        );
        Ok(())
    }
}
