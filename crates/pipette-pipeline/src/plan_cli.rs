//! Operator-facing plan administration: the logic behind `pipette-mgmt plans`.
//! In the library so the API's plan filing and plan read can share it; the
//! binary only parses arguments and renders what these functions return.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;

use pipette_core::client::{Client, ClientStatus};
use pipette_core::matching;
use pipette_core::matching::{NarrowingClauses, describe_clauses};
use pipette_core::plan::{
    PlanExplanation, PlanLeases, PlanManifest, PlanNotes, PlanSource, StarvedExplanation,
    StarvedGroup,
};
use pipette_core::todo_filename::{Lease, parse_leased_key};
use pipette_core::vault::VaultOwner;
use pipette_plan_types::{
    CampaignId, ClientId, FleetStanding, JobCensus, JobId, NonEmptyTrimmedString, PlanId,
    PlanStatus, SchedulingState, UserId,
};
use pipette_store::{
    ClientStore, PlanStore, Stores, TagStore, TodoStore, UnreadableObjectError, drain_clients,
    drain_plans, warn_unreadable_clients,
};

use crate::plan_document;
use crate::plan_document::{DocumentDryRun, ExpandedDocument, index_labels};
use crate::plan_ingestion::{
    CampaignMembership, IngestContext, IngestReport, PlanSubmission, ingest_jobs,
};
use crate::queue_maintenance::{AvailKey, cached_body, collect_all_avail};

/// A plan failed to read, or there is none by that id.
#[derive(Debug, thiserror::Error)]
pub enum PlanLookupError {
    #[error("no plan {plan_id}")]
    NotFound { plan_id: PlanId },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Where the `campaigns` commands read a plan from.
#[derive(Debug, Clone, Copy)]
pub enum PlanInput<'a> {
    Document(&'a Path),
    /// The document a variant edit derives from the plan it revises.
    Text(&'a str),
}

/// Read a plan and file it through the ingestion pipeline into `membership`.
///
/// Validates the `todo/` backend up front: staging needs atomic renames, and
/// failing midway would leave a half-staged plan. Takes no mutate lock, as
/// `plans/` and `todo/` are disjoint from the warehouse paths it serializes.
pub async fn ingest(
    ctx: &IngestContext<'_>,
    input: PlanInput<'_>,
    note: Option<String>,
    change: Option<pipette_plan_types::RevisionChange>,
    membership: CampaignMembership,
) -> anyhow::Result<IngestReport> {
    let note = cli_note(note)?;

    let (jobs, source, plan_warnings) = match input {
        PlanInput::Document(path) => {
            document_parts(expand_plan_file(ctx, path, note.as_ref()).await?)
        }
        PlanInput::Text(text) => {
            document_parts(expand_plan_text(ctx, text.to_string(), note.as_ref()).await?)
        }
    };
    ctx.todo.validate_backend().await?;

    Ok(ingest_jobs(
        ctx,
        PlanSubmission {
            note,
            change,
            // Operator access on the server host authenticates nobody.
            submitted_by: None,
            membership,
            jobs,
            source,
            plan_warnings,
        },
    )
    .await?)
}

/// `campaigns create|ingest --plan <file> --dry-run`: expand and validate a
/// plan document, and write nothing.
pub async fn dry_run_document(
    ctx: &IngestContext<'_>,
    owner: &VaultOwner,
    path: &Path,
    note: Option<String>,
) -> anyhow::Result<DocumentDryRun> {
    let note = cli_note(note)?;
    let expanded = expand_plan_file(ctx, path, note.as_ref()).await?;
    dry_run_expanded(ctx, owner, expanded).await
}

/// [`dry_run_document`] for a document already in memory.
pub async fn dry_run_text(
    ctx: &IngestContext<'_>,
    owner: &VaultOwner,
    text: &str,
    note: Option<String>,
) -> anyhow::Result<DocumentDryRun> {
    let expanded = expand_text(ctx, text, note).await?;
    dry_run_expanded(ctx, owner, expanded).await
}

async fn dry_run_expanded(
    ctx: &IngestContext<'_>,
    owner: &VaultOwner,
    expanded: ExpandedDocument,
) -> anyhow::Result<DocumentDryRun> {
    Ok(plan_document::dry_run(
        ctx,
        owner,
        expanded.jobs,
        expanded.plan_warnings,
        Utc::now(),
    )
    .await?)
}

/// Expand a document that is in memory, with no dry-run check.
pub async fn expand_text(
    ctx: &IngestContext<'_>,
    text: &str,
    note: Option<String>,
) -> anyhow::Result<ExpandedDocument> {
    let note = cli_note(note)?;
    expand_plan_text(ctx, text.to_string(), note.as_ref()).await
}

/// An empty `--note` flag is an error, because an operator who types the flag
/// makes a mistake. An empty `note` from an HTTP client means no note, because
/// a client may send an empty value as its default. A NUL character is also an
/// error, because a PostgreSQL `text` column cannot hold it.
pub(crate) fn cli_note(note: Option<String>) -> anyhow::Result<Option<NonEmptyTrimmedString>> {
    note.map(|note| {
        anyhow::ensure!(
            !note.contains('\0'),
            "--note must not contain a NUL character"
        );
        NonEmptyTrimmedString::try_new(note)
            .map_err(|_| anyhow::anyhow!("--note must not be empty"))
    })
    .transpose()
}

async fn expand_plan_file(
    ctx: &IngestContext<'_>,
    path: &Path,
    note: Option<&NonEmptyTrimmedString>,
) -> anyhow::Result<ExpandedDocument> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading plan document {}", path.display()))?;
    expand_plan_text(ctx, text, note)
        .await
        .with_context(|| format!("expanding plan document {}", path.display()))
}

async fn expand_plan_text(
    ctx: &IngestContext<'_>,
    text: String,
    note: Option<&NonEmptyTrimmedString>,
) -> anyhow::Result<ExpandedDocument> {
    let note = note.map(NonEmptyTrimmedString::as_str);
    let types = plan_document::benchmark_types(ctx.catalog);
    Ok(plan_document::expand_document(ctx.max_jobs, types, note, text).await?)
}

/// An expanded document as [`ingest`] files it: each job labelled by its index,
/// and the source stored beside the plan.
fn document_parts(
    expanded: ExpandedDocument,
) -> (Vec<(String, Value)>, Option<PlanSource>, Vec<String>) {
    (
        index_labels(expanded.jobs),
        Some(expanded.source),
        expanded.plan_warnings,
    )
}

/// The only signal that a listing is short. Stderr, so `--json` stays
/// machine-readable.
fn warn_unreadable(unreadable: &[UnreadableObjectError]) {
    unreadable
        .iter()
        .for_each(|bad| eprintln!("warning: unreadable plan manifest: {bad}"));
}

/// `plans list`: every plan, newest first.
pub async fn list_plans(
    plans: &dyn PlanStore,
    status: Option<PlanStatus>,
    submitter: Option<&UserId>,
    campaign_id: Option<&CampaignId>,
) -> anyhow::Result<Vec<PlanManifest>> {
    let listing = drain_plans(plans, status).await.context("listing plans")?;
    warn_unreadable(&listing.unreadable);
    let mut manifests = listing.items;
    // A plan with no submitter and no campaign shows only in an unfiltered listing.
    if let Some(submitter) = submitter {
        manifests.retain(|p| p.submitted_by.as_ref() == Some(submitter));
    }
    if let Some(campaign_id) = campaign_id {
        manifests.retain(|p| p.campaign_id.as_ref() == Some(campaign_id));
    }
    manifests.sort_by_key(|p| Reverse(p.created_at));
    Ok(manifests)
}

/// `plans status`: the plan's manifest, its notes and its cancel marker. The
/// progress numbers are what `queue-maintenance`, their sole writer, last
/// computed.
pub async fn plan_status(
    plans: &dyn PlanStore,
    plan_id: &PlanId,
) -> Result<PlanRecord, PlanLookupError> {
    let manifest = read_manifest(plans, plan_id).await?;
    let notes = plans
        .get_plan_notes(plan_id)
        .await
        .with_context(|| format!("reading the notes of {plan_id}"))?;
    let cancel_requested = plans
        .has_cancel_marker(plan_id)
        .await
        .with_context(|| format!("checking cancel marker for {plan_id}"))?;
    Ok(PlanRecord {
        manifest,
        notes,
        cancel_requested,
    })
}

/// A plan record holds the manifest of a plan, its notes, and the cancel state.
/// `cancel_requested` is true if a cancel waits for the next maintenance pass.
/// Every status surface shows these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRecord {
    pub manifest: PlanManifest,
    pub notes: PlanNotes,
    pub cancel_requested: bool,
}

/// Shared so the CLI and the HTTP routes report an absent manifest the same way.
async fn read_manifest(
    plans: &dyn PlanStore,
    plan_id: &PlanId,
) -> Result<PlanManifest, PlanLookupError> {
    plans
        .get_plan(plan_id)
        .await
        .with_context(|| format!("reading manifest for {plan_id}"))?
        .ok_or_else(|| PlanLookupError::NotFound {
            plan_id: plan_id.clone(),
        })
}

/// `plans status --leases` — which clients hold the plan's jobs.
///
/// The lists are live, while the snapshot's `running` count is cached. Holders
/// are named by `client_id`: resolving device names would read every client.
pub async fn plan_leases(
    stores: &Stores,
    plan_id: &PlanId,
    now: DateTime<Utc>,
) -> Result<(PlanRecord, PlanLeases), PlanLookupError> {
    let record = plan_status(stores.plans.as_ref(), plan_id).await?;
    let members: HashSet<JobId> = record.manifest.job_ids.iter().cloned().collect();
    let (leases, unparseable_keys) = collect_plan_leases(stores.todo.as_ref(), &members).await?;
    Ok((record, PlanLeases::new(leases, unparseable_keys, now)))
}

/// Every lease held on a job in `members`, beside the count of `leased/` keys
/// that do not parse (they name no job, so the reports only count them).
async fn collect_plan_leases(
    todo: &dyn TodoStore,
    members: &HashSet<JobId>,
) -> anyhow::Result<(Vec<Lease>, usize)> {
    Ok(todo
        .list_leased()
        .await
        .context("listing leases")?
        .iter()
        .fold(
            (Vec::new(), 0usize),
            |(mut leases, skipped), key| match parse_leased_key(key) {
                Ok(lease) => {
                    if members.contains(&lease.job_id) {
                        leases.push(lease);
                    }
                    (leases, skipped)
                }
                Err(_) => (leases, skipped + 1),
            },
        ))
}

/// `plans status --explain` — why the plan's jobs are not scheduled.
///
/// Clause sets are cached from the last `queue-maintenance` run; the fleet is
/// live. A client record the store cannot read is an error, not a smaller fleet,
/// so the report never invents a starvation.
pub async fn explain_plan(
    stores: &Stores,
    plan_id: &PlanId,
    now: DateTime<Utc>,
) -> Result<(PlanRecord, PlanExplanation), PlanLookupError> {
    explain_plan_with(
        stores.plans.as_ref(),
        stores.todo.as_ref(),
        stores.clients.as_ref(),
        stores.tags.as_ref(),
        plan_id,
        now,
    )
    .await
}

/// [`explain_plan`] over the four stores it reads, for an HTTP handler, which
/// holds no `Stores`.
pub async fn explain_plan_with(
    plans: &dyn PlanStore,
    todo: &dyn TodoStore,
    clients: &dyn ClientStore,
    tags: &dyn TagStore,
    plan_id: &PlanId,
    now: DateTime<Utc>,
) -> Result<(PlanRecord, PlanExplanation), PlanLookupError> {
    let record = plan_status(plans, plan_id).await?;
    let manifest = &record.manifest;
    let cancel_requested = record.cancel_requested;
    let plan_id = manifest.plan_id.clone();

    let members: HashSet<JobId> = manifest.job_ids.iter().cloned().collect();

    // ── the queue ───────────────────────────────────────────────────────────
    let listing = collect_all_avail(todo)
        .await
        .context("listing the available-job queue")?;
    let avail: Vec<&AvailKey> = listing
        .keys
        .iter()
        .filter(|key| members.contains(&key.job_id))
        .collect();
    // The claim path refuses an expired job even before `queue-maintenance`
    // sweeps it, so an expired entry is available but not claimable.
    let claimable: Vec<&AvailKey> = avail
        .iter()
        .copied()
        .filter(|key| !key.expires_at.is_expired(now))
        .collect();

    // Not the `eligible/` tree: that is hundreds of thousands of keys.
    let indexed_keys = todo
        .read_eligible_indexed()
        .await
        .context("reading the eligible-index state")?;
    let indexed = claimable
        .iter()
        .filter(|key| indexed_keys.contains(&key.key))
        .count();

    // A job under an unparseable key reads as `gone`; the count is the caveat
    // on that figure, as the key names no job to subtract.
    let (leases, unparseable_leased_keys) = collect_plan_leases(todo, &members).await?;
    let held = PlanLeases::new(leases, unparseable_leased_keys, now);
    let leased = held.live.len() + held.lapsed.len();

    let denied: HashMap<JobId, HashSet<ClientId>> = todo
        .list_all_denied()
        .await
        .context("listing denial markers")?
        .into_iter()
        .filter(|(job_id, _)| members.contains(job_id))
        .fold(HashMap::new(), |mut denied, (job_id, client_id)| {
            denied.entry(job_id).or_default().insert(client_id);
            denied
        });

    // ── the fleet ───────────────────────────────────────────────────────────
    let fleet = drain_clients(clients).await.context("listing clients")?;
    warn_unreadable_clients(&fleet.unreadable);
    let suspended: HashSet<ClientId> = todo
        .list_suspensions()
        .await
        .context("listing suspensions")?
        .into_iter()
        .map(|(client_id, _)| client_id)
        .collect();
    let pending_reindex: HashSet<ClientId> = todo
        .list_pending_reindex()
        .await
        .context("listing pending-reindex flags")?
        .into_iter()
        .map(|(client_id, _)| client_id)
        .collect();

    // Every count below `approved` is measured against the claim permission.
    let rights = matching::FleetRights::new(
        clients
            .list_client_permissions()
            .await
            .context("listing client permissions")?,
    );
    let fleet_tags = matching::FleetTags::new(
        tags.list_forward_tag_markers()
            .await
            .context("listing client tags")?,
    );
    let registered: Vec<&Client> = fleet.items.iter().collect();
    let approved: Vec<&Client> = registered
        .iter()
        .copied()
        .filter(|client| client.status == ClientStatus::Approved)
        .collect();
    let workers: Vec<&Client> = rights.workers(registered.iter().copied());
    let unblocked: Vec<&Client> = workers
        .iter()
        .copied()
        .filter(|client| {
            !suspended.contains(&client.client_id) && !pending_reindex.contains(&client.client_id)
        })
        .collect();
    let fleet_standing = FleetStanding {
        registered: registered.len(),
        approved: approved.len(),
        working: workers.len(),
        suspended: workers
            .iter()
            .filter(|client| suspended.contains(&client.client_id))
            .count(),
        pending_reindex: workers
            .iter()
            .filter(|client| pending_reindex.contains(&client.client_id))
            .count(),
        unblocked: unblocked.len(),
    };

    // ── the cached starved groups ───────────────────────────────────────────
    let starved: &[StarvedGroup] = manifest
        .progress_snapshot
        .as_ref()
        .map_or(&[], |snapshot| &snapshot.starved);
    let available: HashMap<&JobId, &AvailKey> =
        avail.iter().map(|key| (&key.job_id, *key)).collect();
    // Shared with the verdict scan, which may reach a group's representative job.
    let mut bodies: HashMap<String, Value> = HashMap::new();
    let mut groups = Vec::with_capacity(starved.len());
    for group in starved {
        groups.push(
            explain_group(
                todo,
                group,
                &available,
                GroupFleet {
                    registered: &registered,
                    approved: &approved,
                    workers: &workers,
                    rights: &rights,
                    tags: &fleet_tags,
                },
                &mut bodies,
            )
            .await?,
        );
    }

    // Before the first `queue-maintenance` pass there is no snapshot, so the
    // ingestion warnings are the only starvation signal (they yield no ladder).
    let starved_ids: HashSet<&JobId> = match &manifest.progress_snapshot {
        Some(snapshot) => snapshot
            .starved
            .iter()
            .flat_map(|group| group.job_ids.iter())
            .collect(),
        None => manifest
            .warnings
            .iter()
            .flat_map(|warning| warning.job_ids.iter())
            .collect(),
    };
    // An empty claimable set is settled by the `Expired` gate, so the vacuous
    // `all` never reaches the verdict.
    let all_claimable_starved = claimable
        .iter()
        .all(|key| starved_ids.contains(&key.job_id));
    let census = JobCensus {
        total: manifest.job_ids.len(),
        available: avail.len(),
        expired: avail.len() - claimable.len(),
        claimable: claimable.len(),
        indexed,
        leased,
        lapsed_leases: held.lapsed.len(),
        // A job in the recycle race sits in both queues, hence the floor.
        gone: manifest.job_ids.len().saturating_sub(avail.len() + leased),
        denied: claimable
            .iter()
            .filter(|key| denied.contains_key(&key.job_id))
            .count(),
        unparseable_avail_keys: listing.skipped,
        unparseable_leased_keys: held.unparseable_keys,
    };
    let gated = decide_scheduling_state(SchedulingInputs {
        status: manifest.status,
        cancel_requested,
        census: &census,
        all_claimable_starved,
    });
    // The cached starvation set can miss a job that starved since the last pass,
    // so a scan that matches no working client answers `Starved` too.
    let verdict = match gated {
        Some(verdict) => verdict,
        None => match scan_takeability(
            todo,
            &claimable,
            &starved_ids,
            ScanFleet {
                working: &workers,
                unblocked: &unblocked,
                denied: &denied,
                tags: &fleet_tags,
            },
            &mut bodies,
            now,
        )
        .await?
        {
            Takeability::Takeable => SchedulingState::Schedulable,
            Takeability::Blocked => SchedulingState::ClientsBlocked,
            Takeability::NoMatch => SchedulingState::Starved,
            Takeability::Vanished => SchedulingState::QueueMoved,
        },
    };

    let explanation = PlanExplanation {
        plan_id,
        read_at: now,
        snapshot_computed_at: manifest.progress_snapshot.as_ref().map(|s| s.computed_at),
        census,
        fleet: fleet_standing,
        groups,
        starved_by_live_scan: gated.is_none() && verdict == SchedulingState::Starved,
        verdict,
        leases: held,
    };
    Ok((record, explanation))
}

/// The verdict reads the [`JobCensus`] the report prints, so the two can never
/// describe different queues.
struct SchedulingInputs<'a> {
    status: PlanStatus,
    cancel_requested: bool,
    census: &'a JobCensus,
    /// The cached starvation set names every claimable job.
    all_claimable_starved: bool,
}

/// The first gate that blocks every outstanding job, as far as the counts
/// alone can answer; a partly blocked plan reads as the later, milder gate.
///
/// `None` leaves the verdict to [`scan_takeability`], so the body reads stay
/// off a plan the cheap gates already settle.
fn decide_scheduling_state(input: SchedulingInputs<'_>) -> Option<SchedulingState> {
    if input.status == PlanStatus::Creating {
        return Some(SchedulingState::StillCreating);
    }
    if input.cancel_requested || input.status == PlanStatus::Cancelling {
        return Some(SchedulingState::CancelPending);
    }
    // One live lease beside the dead ones is work in flight, so the gate below.
    if input.census.available == 0
        && input.census.leased > 0
        && input.census.lapsed_leases == input.census.leased
    {
        return Some(SchedulingState::LeasesLapsed);
    }
    if input.census.available == 0 {
        return Some(SchedulingState::NothingOutstanding);
    }
    if input.census.claimable == 0 {
        return Some(SchedulingState::Expired);
    }
    // Before starvation: until the first `queue-maintenance` run there is no
    // `eligible/` marker and no snapshot to report starvation.
    if input.census.indexed == 0 {
        return Some(SchedulingState::NotIndexed);
    }
    if input.all_claimable_starved {
        return Some(SchedulingState::Starved);
    }
    None
}

/// What the live body scan found across a plan's claimable jobs.
///
/// The two negative answers split a clause edit (`NoMatch`) from a fleet that
/// needs an approval, unsuspension or cleared denial (`Blocked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takeability {
    Takeable,
    Blocked,
    NoMatch,
    /// The scan read no body at all, so it has no finding to report.
    Vanished,
}

/// Every job id under a live lease, read fresh: the listing `explain_plan`
/// holds predates the scan, so it cannot tell a mid-scan claim from a teardown.
/// A lapsed lease is not proof of a claim.
async fn leased_job_ids(
    todo: &dyn TodoStore,
    now: DateTime<Utc>,
) -> anyhow::Result<HashSet<JobId>> {
    Ok(todo
        .list_leased()
        .await
        .context("listing leases to confirm a claim")?
        .iter()
        .filter_map(|key| parse_leased_key(key).ok())
        .filter(|lease| lease.lease_expiry > now)
        .map(|lease| lease.job_id)
        .collect())
}

/// Named fields: two `&[&Client]` slices transpose silently at a call site.
#[derive(Clone, Copy)]
struct ScanFleet<'a> {
    /// Approved and holding `perform_work`, whatever its standing. A job that
    /// matches none of these is starved.
    working: &'a [&'a Client],
    /// Not suspended, and not pending an eligible-index rebuild.
    unblocked: &'a [&'a Client],
    denied: &'a HashMap<JobId, HashSet<ClientId>>,
    tags: &'a matching::FleetTags,
}

/// Which clients can take a plan's claimable jobs now, read from the job bodies.
///
/// Weighs each job against the clients that match it, never fleet-wide, so a
/// job whose only match is suspended cannot read as schedulable. Starvation is
/// against the working fleet: such a job is blocked, not starved.
///
/// Reads non-starved jobs first and stops at the first takeable one. A job
/// that left `avail/` and sits under a lease proves a claim; a body read
/// failure propagates rather than invent "no client can take these".
async fn scan_takeability(
    todo: &dyn TodoStore,
    claimable: &[&AvailKey],
    starved_ids: &HashSet<&JobId>,
    fleet: ScanFleet<'_>,
    bodies: &mut HashMap<String, Value>,
    now: DateTime<Utc>,
) -> anyhow::Result<Takeability> {
    let ScanFleet {
        working,
        unblocked,
        denied,
        tags,
    } = fleet;
    // Not on an empty `unblocked` set: the scan must still tell a blocked fleet
    // from jobs no working client matches.
    if working.is_empty() {
        return Ok(Takeability::Blocked);
    }
    let (fresh, stale): (Vec<&&AvailKey>, Vec<&&AvailKey>) = claimable
        .iter()
        .partition(|key| !starved_ids.contains(&key.job_id));
    let mut working_match = false;
    let mut examined = 0usize;
    // Read once, on the first vanish.
    let mut leased: Option<HashSet<JobId>> = None;
    for key in fresh.into_iter().chain(stale) {
        let Some(body) = cached_body(todo, key, bodies)
            .await
            .with_context(|| format!("reading the body of {}", key.job_id))?
        else {
            if leased.is_none() {
                leased = Some(leased_job_ids(todo, now).await?);
            }
            if leased.as_ref().is_some_and(|ids| ids.contains(&key.job_id)) {
                return Ok(Takeability::Takeable);
            }
            continue;
        };
        let takeable = unblocked.iter().any(|client| {
            matching::job_matches_client(&body, client, tags)
                && !denied
                    .get(&key.job_id)
                    .is_some_and(|ids| ids.contains(&client.client_id))
        });
        if takeable {
            return Ok(Takeability::Takeable);
        }
        examined += 1;
        working_match = working_match
            || working
                .iter()
                .any(|client| matching::job_matches_client(&body, client, tags));
    }
    match (examined, working_match) {
        (0, _) => Ok(Takeability::Vanished),
        (_, true) => Ok(Takeability::Blocked),
        (_, false) => Ok(Takeability::NoMatch),
    }
}

/// Named fields: three same-typed slices transpose silently at a call site.
#[derive(Clone, Copy)]
struct GroupFleet<'a> {
    registered: &'a [&'a Client],
    approved: &'a [&'a Client],
    /// The approved clients that also hold `perform_work`.
    workers: &'a [&'a Client],
    rights: &'a matching::FleetRights,
    tags: &'a matching::FleetTags,
}

/// Explain one cached starved group against the live fleet.
///
/// The ladder narrows the working fleet, so a missing grant never reads as a
/// clause to edit.
async fn explain_group(
    todo: &dyn TodoStore,
    group: &StarvedGroup,
    available: &HashMap<&JobId, &AvailKey>,
    fleet: GroupFleet<'_>,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<StarvedExplanation> {
    let GroupFleet {
        registered,
        approved,
        workers,
        rights,
        tags,
    } = fleet;
    let clauses = NarrowingClauses::from(group);
    // One body answers both the shape verdict and the match counts, so they
    // agree. With none left in `avail/`, the clause rebuild stands in.
    let (body, never_matches) = match read_group_body(todo, group, available, bodies).await? {
        Some(read) => read,
        None => (clauses.body(), None),
    };
    let matches = |pool: &[&Client]| {
        pool.iter()
            .filter(|client| matching::job_matches_client(&body, client, tags))
            .count()
    };
    Ok(StarvedExplanation {
        description: describe_clauses(&group.clauses),
        job_count: group.job_ids.len(),
        // A ladder under an unmatchable body would point at the wrong clause.
        ladder: match never_matches {
            Some(_) => Vec::new(),
            None => matching::narrow_fleet(clauses, workers, tags),
        },
        never_matches,
        roster: (!group.clauses.clients.is_empty()).then(|| {
            // `registered`, so unapproved and ungranted ids show.
            matching::roster_standing(clauses, registered, rights, tags)
        }),
        // The drop between nested pools is the action: approve, grant, or edit.
        live_working_client_count: matches(workers),
        live_approved_client_count: matches(approved),
        live_registered_client_count: matches(registered),
    })
}

/// Read one real job body for the group, with the rule that proves no client
/// can ever match it.
///
/// Not a body rebuilt from the group: `requirement_key` records a malformed
/// clause as an absent one, so the rebuild would name the wrong fault.
async fn read_group_body(
    todo: &dyn TodoStore,
    group: &StarvedGroup,
    available: &HashMap<&JobId, &AvailKey>,
    bodies: &mut HashMap<String, Value>,
) -> anyhow::Result<Option<(Value, Option<String>)>> {
    let Some(key) = group
        .job_ids
        .iter()
        .find_map(|job_id| available.get(job_id).copied())
    else {
        return Ok(None);
    };
    let Some(body) = cached_body(todo, key, bodies)
        .await
        .with_context(|| format!("reading the job body of {}", key.job_id))?
    else {
        return Ok(None);
    };
    // Shape first, so a malformed clause is reported as a shape fault rather
    // than as whichever contradiction the lenient read invents.
    let never_matches = if matching::capability_clauses_malformed(&body) {
        Some(
            "a capability clause is not a well-formed array of strings, so the job matches nobody"
                .to_owned(),
        )
    } else {
        matching::unmatchable_reason(&body)
    };
    Ok(Some((body, never_matches)))
}

/// What [`cancel_plan`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum CancelOutcome {
    Requested,
    AlreadyCancelled,
    AlreadyCancelling,
    /// Not an error: the CLI exits non-zero, the API answers `409`.
    AlreadyComplete,
}

impl CancelOutcome {
    /// The wire word of the API's `outcome` field.
    pub fn label(self) -> &'static str {
        self.into()
    }
}

/// `plans cancel` — record a cancellation request for a plan.
///
/// Writes only the marker: a `status` write here would race the
/// `queue-maintenance` pass that owns the latch and teardown. The refusals are
/// feedback, not invariants; the pass collects a stray marker.
pub async fn cancel_plan(
    plans: &dyn PlanStore,
    plan_id: &PlanId,
) -> Result<CancelOutcome, PlanLookupError> {
    let manifest = read_manifest(plans, plan_id).await?;
    Ok(request_cancel(plans, &manifest).await?)
}

/// Which statuses are cancellable, shared by `plans cancel` and the API.
pub async fn request_cancel(
    plans: &dyn PlanStore,
    manifest: &PlanManifest,
) -> anyhow::Result<CancelOutcome> {
    let plan_id = &manifest.plan_id;
    match manifest.status {
        PlanStatus::Complete => Ok(CancelOutcome::AlreadyComplete),
        // Already latched: the pass drives teardown from the status itself.
        PlanStatus::Cancelled => Ok(CancelOutcome::AlreadyCancelled),
        PlanStatus::Cancelling => Ok(CancelOutcome::AlreadyCancelling),
        // `creating` on purpose: it shares the stranded-ingest teardown.
        PlanStatus::Creating | PlanStatus::Active | PlanStatus::PendingClients => {
            plans
                .write_cancel_marker(plan_id)
                .await
                .with_context(|| format!("writing cancel marker for {plan_id}"))?;
            tracing::info!(plan_id = %plan_id, "plan cancellation requested");
            Ok(CancelOutcome::Requested)
        }
    }
}

#[cfg(test)]
mod tests {
    const USER_ADA: &str = "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
    const USER_BEA: &str = "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7f";

    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{DateTime, Duration, Utc};
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::benchmark::Benchmark;
    use pipette_core::config::{Config, StorageConfig};
    use pipette_core::denial::DenialRecord;
    use pipette_core::plan::PlanManifest;
    use pipette_plan_types::BenchmarkId;
    use pipette_plan_types::FleetDetail;
    use pipette_plan_types::JobId;
    use pipette_store_fs::build_local_fs_stores;

    use super::*;
    use crate::plan_ingestion::ModelCheck;

    fn stores_in(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = Config {
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..Config::default()
        };
        build_local_fs_stores(&config)
    }

    fn manifest(id: u128, status: PlanStatus) -> anyhow::Result<PlanManifest> {
        let created_at: DateTime<Utc> = "2026-07-20T17:55:00Z".parse()?;
        Ok(PlanManifest {
            campaign_id: None,
            previous_plan_id: None,
            source_sha256: None,
            submitted_sha256: None,
            plan_warnings: Vec::new(),
            plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(id)),
            status,
            // Stagger by id so "newest first" has a deterministic expectation.
            created_at: created_at + Duration::seconds(i64::try_from(id)?),
            submitted_by: None,
            benchmark_types: Default::default(),
            job_ids: vec![JobId::new_unchecked("job-1")],
            content_keys: Default::default(),
            rerun_of: Default::default(),
            warnings: Vec::new(),
            expired_jobs: BTreeMap::new(),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: None,
            terminal_at: None,
        })
    }

    // ── ingest ──────────────────────────────────────────────────────────────

    fn catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let def = "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 100";
        let bench = Benchmark::from_toml("bench-1", def)?;
        Ok(HashMap::from([(BenchmarkId::try_new("bench-1")?, bench)]))
    }

    /// The config default; every queue here is fresh, so any plausible value works.
    const CREATING_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(21_600);
    const MAX_JOBS: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(5000) {
        Some(n) => n,
        None => std::num::NonZeroUsize::MIN,
    };

    // ── ingest --plan ───────────────────────────────────────────────────────

    /// Expands to three cells over the two [`scheduler_catalog`] benchmarks.
    const AFM_MLX: &str = include_str!("../../../examples/plans/scheduler/afm-mlx.toml");

    /// A one-variant macOS MLX document. `lines` sets the variant's eligibility.
    fn mlx_document(lines: &str) -> String {
        format!(
            r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
{lines}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" }} }}]
"#
        )
    }

    fn scheduler_catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        [
            (
                "decode_throughput_512_100",
                "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 512\n\
                 parameter_decode_tokens = 100",
            ),
            (
                "end_to_end_latency_512_256",
                "benchmark_type = \"end_to_end_latency\"\nparameter_prefill_tokens = 512\n\
                 parameter_decode_tokens = 256",
            ),
        ]
        .into_iter()
        .map(|(id, def)| Ok((BenchmarkId::try_new(id)?, Benchmark::from_toml(id, def)?)))
        .collect()
    }

    fn plan_file(dir: &Path, document: &str) -> anyhow::Result<std::path::PathBuf> {
        let path = dir.join("plan.toml");
        std::fs::write(&path, document)?;
        Ok(path)
    }

    async fn ingest_test_document(
        stores: &Stores,
        path: &Path,
        max_jobs: std::num::NonZeroUsize,
        note: Option<String>,
    ) -> anyhow::Result<IngestReport> {
        let catalog = scheduler_catalog()?;
        let ctx = IngestContext::from_stores(
            stores,
            &catalog,
            ModelCheck::none(),
            max_jobs,
            CREATING_MAX_AGE,
        );
        super::ingest(
            &ctx,
            PlanInput::Document(path),
            note,
            None,
            CampaignMembership::for_test(UserId::try_new("user_test")?),
        )
        .await
    }

    async fn assert_nothing_written(stores: &Stores, store_dir: &Path) -> anyhow::Result<()> {
        assert!(
            drain_plans(stores.plans.as_ref(), None)
                .await?
                .items
                .is_empty()
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        assert!(stores.plans.list_pending_source_markers().await?.is_empty());
        let sources = std::fs::read_dir(store_dir.join("plan-sources"))
            .map(Iterator::count)
            .unwrap_or_default();
        assert_eq!(sources, 0, "no plan source is stored");
        assert_eq!(
            pipette_store_testkit::local_fs_job_record_count(store_dir),
            0,
            "no job source or link is stored"
        );
        Ok(())
    }

    /// Cells are labelled by index, the manifest names the source by digest,
    /// and `--note` is trimmed.
    #[tokio::test]
    async fn ingest_document_stages_its_cells_and_stores_the_source() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let path = plan_file(plans_dir.path(), AFM_MLX)?;

        let report =
            ingest_test_document(&stores, &path, MAX_JOBS, Some("  smoke  ".to_string())).await?;
        assert_eq!(
            report.note.as_ref().map(NonEmptyTrimmedString::as_str),
            Some("smoke")
        );
        assert_eq!(report.job_count, 3);
        assert_eq!(
            report
                .job_ids
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            vec!["0", "1", "2"]
        );
        assert!(
            report.plan_warnings.is_empty(),
            "{:?}",
            report.plan_warnings
        );
        assert_eq!(stores.todo.list_avail().await?.len(), 3);

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("the manifest exists")?;
        let source = stores
            .plans
            .get_plan_source(&report.plan_id)
            .await?
            .context("the source exists")?;
        assert_eq!(source.text(), AFM_MLX, "stored byte for byte");
        assert_eq!(manifest.source_sha256.as_ref(), Some(source.sha256()));
        assert!(stores.plans.list_pending_source_markers().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn ingest_document_twice_shares_the_jobs_of_the_first_plan() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let path = plan_file(plans_dir.path(), AFM_MLX)?;

        let first = ingest_test_document(&stores, &path, MAX_JOBS, None).await?;
        let second = ingest_test_document(&stores, &path, MAX_JOBS, None).await?;
        assert_ne!(first.plan_id, second.plan_id);
        assert_eq!(second.merged_job_count, 3);
        assert_eq!(second.job_ids, first.job_ids);
        assert_eq!(stores.todo.list_avail().await?.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn ingest_document_stores_the_planner_warnings() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        // A dead capability path with a live roster: a warning, not a refusal.
        let document = mlx_document(
            "requires = [\"os:macos\"]\nclients = [\"ev1_live\"]\nexcludes = [\"os:macos\"]",
        );
        let path = plan_file(plans_dir.path(), &document)?;

        let report = ingest_test_document(&stores, &path, MAX_JOBS, None).await?;
        assert_eq!(report.plan_warnings.len(), 1, "{:?}", report.plan_warnings);
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("the manifest exists")?;
        assert_eq!(manifest.plan_warnings, report.plan_warnings);
        Ok(())
    }

    #[rstest]
    #[case::hf_token_env(
        r#"benchmarks = ["prefill_throughput_512"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", envs = ["HF_TOKEN=hf_supersecret"] }]
"#.to_string(),
        MAX_JOBS,
        "HF_TOKEN"
    )]
    #[case::over_the_ceiling(AFM_MLX.to_string(), std::num::NonZeroUsize::MIN, "plan_max_jobs")]
    #[case::not_toml("benchmarks = [".to_string(), MAX_JOBS, "not a valid scheduler plan")]
    #[tokio::test]
    async fn ingest_document_refused_writes_nothing(
        #[case] document: String,
        #[case] max_jobs: std::num::NonZeroUsize,
        #[case] needle: &str,
    ) -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let path = plan_file(plans_dir.path(), &document)?;

        let err = format!(
            "{:#}",
            ingest_test_document(&stores, &path, max_jobs, None)
                .await
                .err()
                .context("ingest must reject this document")?
        );
        assert!(err.contains(needle), "{needle:?} not in {err}");
        assert!(err.contains("plan.toml"), "names the file: {err}");
        assert!(!err.contains("hf_supersecret"), "echoes the token: {err}");
        assert_nothing_written(&stores, store_dir.path()).await
    }

    #[rstest]
    #[case::empty("   ", "must not be empty")]
    #[case::nul("a\0b", "must not contain a NUL character")]
    #[tokio::test]
    async fn ingest_document_rejects_a_bad_note(
        #[case] note: &str,
        #[case] needle: &str,
    ) -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let path = plan_file(plans_dir.path(), AFM_MLX)?;

        let err = ingest_test_document(&stores, &path, MAX_JOBS, Some(note.to_string()))
            .await
            .err()
            .context("ingest must reject this --note")?
            .to_string();
        assert!(err.contains(needle), "{needle:?} not in {err}");
        assert_nothing_written(&stores, store_dir.path()).await
    }

    /// The operator transport runs the same vault check as the route.
    #[tokio::test]
    async fn a_vault_reference_nothing_backs_writes_nothing() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let document = r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "gguf_text", source = "vault_url", url = "/vault/resources/vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e/content", principal_id = "org_0195c3e2" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
"#;
        let path = plan_file(plans_dir.path(), document)?;

        let err = format!(
            "{:#}",
            ingest_test_document(&stores, &path, MAX_JOBS, None)
                .await
                .err()
                .context("ingest must reject this document")?
        );
        assert!(err.contains("does not exist"), "{err}");
        assert_nothing_written(&stores, store_dir.path()).await
    }

    /// The refusal message never holds the token.
    #[tokio::test]
    async fn a_model_token_without_an_import_writes_nothing() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let document = mlx_document("requires = [\"os:macos\"]").replace(
            "repo_name = \"LFM2.5-350M-MLX-4bit\"",
            "repo_name = \"LFM2.5-350M-MLX-4bit\", auth_token = \"hf_supersecret\"",
        );
        let path = plan_file(plans_dir.path(), &document)?;

        let err = format!(
            "{:#}",
            ingest_test_document(&stores, &path, MAX_JOBS, None)
                .await
                .err()
                .context("ingest must reject this document")?
        );
        assert!(err.contains("cannot import a model"), "{err}");
        assert!(!err.contains("hf_supersecret"), "echoes the token: {err}");
        assert_nothing_written(&stores, store_dir.path()).await
    }

    #[tokio::test]
    async fn dry_run_document_writes_nothing() -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let plans_dir = tempfile::tempdir()?;
        let path = plan_file(plans_dir.path(), AFM_MLX)?;
        let catalog = scheduler_catalog()?;
        let ctx = IngestContext::from_stores(
            &stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );

        let owner = VaultOwner::User {
            user_id: UserId::try_new("user_smoke")?,
        };
        let report = dry_run_document(&ctx, &owner, &path, Some("smoke".to_string()))
            .await?
            .report;
        assert_eq!(report.bodies.len(), 3);
        assert!(report.imports.is_empty());
        assert!(report.plan_warnings.is_empty());
        assert_nothing_written(&stores, store_dir.path()).await
    }

    /// A dry run refuses a vLLM eval cell pinned off its entry's context bound.
    #[rstest]
    #[case::at_the_bound(32768, None)]
    #[case::elsewhere(
        40960,
        Some(
            "`spec` cannot run eval_math_500_2026.10.1_ctx32768 as the catalog defines it: \
             max_model_len = 40960 is not the benchmark's parameter_max_context = 32768"
        )
    )]
    #[tokio::test]
    async fn a_dry_run_refuses_an_eval_cell_that_cannot_hold_its_bound(
        #[case] max_model_len: u32,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let store_dir = tempfile::tempdir()?;
        let stores = stores_in(store_dir.path())?;
        let id = "eval_math_500_2026.10.1_ctx32768";
        let entry = "benchmark_type = \"eval\"\nparameter_eval_id = \"math_500\"\n\
                     parameter_dataset_name = \"2026.10.1\"\nparameter_max_tokens = 32768\n\
                     parameter_max_context = 32768";
        let catalog =
            HashMap::from([(BenchmarkId::try_new(id)?, Benchmark::from_toml(id, entry)?)]);
        let ctx = IngestContext::from_stores(
            &stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );
        let document = format!(
            r#"benchmarks = ["{id}"]

[[variants]]
requires = ["os:ubuntu", "accel:gpu"]
models = [{{ type = "torch", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-1.2B-Instruct" }}]
runtimes = [{{ type = "docker_vllm", image_name = "vllm/vllm-openai", image_tag = "v0.31.0", flavor = "nvidia_gpu" }}]
runtime_flags = [
  {{ benchmark_type = "eval", runtime_type = "docker_vllm", model_type = "torch", max_model_len = {max_model_len} }},
]
"#
        );
        let owner = VaultOwner::User {
            user_id: UserId::try_new("user_smoke")?,
        };
        let dry_run = dry_run_text(&ctx, &owner, &document, None).await;
        match refusal {
            None => anyhow::ensure!(
                dry_run.as_ref().is_ok_and(|r| r.report.bodies.len() == 1),
                "{dry_run:?}"
            ),
            Some(refusal) => {
                let err = format!("{:#}", dry_run.err().context("must be refused")?);
                anyhow::ensure!(err.contains("(index 0)"), "{err}");
                anyhow::ensure!(err.contains(refusal), "{err}");
            }
        }
        assert_nothing_written(&stores, store_dir.path()).await
    }

    // ── list_plans ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_plans_sorts_newest_first_and_filters() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let old = manifest(1, PlanStatus::Active)?;
        let new = manifest(3, PlanStatus::Complete)?;
        let mid = manifest(2, PlanStatus::Active)?;
        for m in [&old, &new, &mid] {
            stores.plans.put_plan(m).await?;
        }

        let listed = list_plans(stores.plans.as_ref(), None, None, None).await?;
        assert_eq!(
            listed.iter().map(|p| p.created_at).collect::<Vec<_>>(),
            vec![new.created_at, mid.created_at, old.created_at]
        );
        assert_eq!(
            list_plans(
                stores.plans.as_ref(),
                Some(PlanStatus::Complete),
                None,
                None
            )
            .await?,
            vec![new]
        );
        Ok(())
    }

    /// Whole-value match, composed with the status filter; a plan with no
    /// submitter shows only unfiltered.
    #[tokio::test]
    async fn list_plans_filters_by_submitter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let by_user = PlanManifest {
            submitted_by: Some(UserId::try_new(USER_ADA)?),
            ..manifest(1, PlanStatus::Active)?
        };
        let by_client = PlanManifest {
            submitted_by: Some(UserId::try_new(USER_BEA)?),
            ..manifest(2, PlanStatus::Complete)?
        };
        let by_operator = manifest(3, PlanStatus::Active)?;
        for m in [&by_user, &by_client, &by_operator] {
            stores.plans.put_plan(m).await?;
        }

        let cases = [
            (USER_ADA, None, vec![by_user.clone()]),
            (USER_BEA, None, vec![by_client.clone()]),
            // A prefix is not a match.
            (
                USER_ADA
                    .get(..USER_ADA.len() - 1)
                    .context("USER_ADA is ASCII")?,
                None,
                vec![],
            ),
            (USER_ADA, Some(PlanStatus::Complete), vec![]),
            (USER_BEA, Some(PlanStatus::Complete), vec![by_client]),
        ];
        for (submitter, status, expected) in cases {
            let submitter = UserId::try_new(submitter)?;
            assert_eq!(
                list_plans(stores.plans.as_ref(), status, Some(&submitter), None).await?,
                expected,
                "submitter {submitter}, status {status:?}"
            );
        }

        assert!(
            list_plans(stores.plans.as_ref(), None, None, None)
                .await?
                .contains(&by_operator)
        );
        Ok(())
    }

    /// Older manifests carry a client `ev1_…` in `submitted_by`; refusing them
    /// hid the plan from every listing and every `queue-maintenance` pass.
    #[tokio::test]
    async fn a_dated_submitter_lists_and_reads_rather_than_being_unreadable() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(7));
        std::fs::create_dir_all(dir.path().join("plans"))?;
        std::fs::write(
            dir.path()
                .join("plans")
                .join(format!("{}.json", plan_id.as_str())),
            serde_json::to_vec(&json!({
                "plan_id": plan_id.as_str(),
                "status": "active",
                "created_at": "2026-07-20T17:55:00Z",
                "job_ids": ["job-1"],
                "submitted_by": "ev1_filer",
            }))?,
        )?;

        let listing = drain_plans(stores.plans.as_ref(), None).await?;
        assert!(listing.unreadable.is_empty());
        assert_eq!(
            listing.items.iter().map(|p| &p.plan_id).collect::<Vec<_>>(),
            vec![&plan_id]
        );
        assert!(listing.items[0].submitted_by.is_none());

        let submitter = UserId::try_new(USER_ADA)?;
        assert!(
            list_plans(stores.plans.as_ref(), None, Some(&submitter), None)
                .await?
                .is_empty()
        );

        // The addressed read failed outright, not merely warned.
        let record = plan_status(stores.plans.as_ref(), &plan_id).await?;
        assert_eq!(record.manifest.plan_id, plan_id);
        assert!(record.manifest.submitted_by.is_none());
        Ok(())
    }

    // ── plan_status ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn plan_status_absent_plan_errors() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let absent = PlanId::from_uuid(uuid::Uuid::from_u128(9));
        let err = plan_status(stores.plans.as_ref(), &absent)
            .await
            .err()
            .context("plan_status must fail for a missing plan")?;
        assert!(matches!(&err, PlanLookupError::NotFound { plan_id } if *plan_id == absent));
        assert!(err.to_string().contains("no plan"), "{err}");
        Ok(())
    }

    // ── cancel_plan ─────────────────────────────────────────────────────────

    /// `plans status` reports the pending cancel meanwhile.
    #[rstest]
    #[case::active(PlanStatus::Active)]
    #[case::pending_clients(PlanStatus::PendingClients)]
    #[case::creating(PlanStatus::Creating)]
    #[tokio::test]
    async fn cancel_writes_marker_without_touching_status(
        #[case] status: PlanStatus,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, PlanStatus::Active)?;
        let m = PlanManifest { status, ..m };
        stores.plans.put_plan(&m).await?;

        let outcome = cancel_plan(stores.plans.as_ref(), &m.plan_id).await?;
        assert_eq!(outcome, CancelOutcome::Requested);

        assert!(stores.plans.has_cancel_marker(&m.plan_id).await?);
        let after = stores
            .plans
            .get_plan(&m.plan_id)
            .await?
            .context("manifest still present")?;
        assert_eq!(after.status, status, "cancel does not latch the status");
        assert_eq!(after, m, "the manifest is not rewritten at all");

        let record = plan_status(stores.plans.as_ref(), &m.plan_id).await?;
        assert!(record.cancel_requested);
        assert_eq!(record.manifest.status, status);
        Ok(())
    }

    #[rstest]
    #[case::cancelling(PlanStatus::Cancelling, CancelOutcome::AlreadyCancelling)]
    #[case::cancelled(PlanStatus::Cancelled, CancelOutcome::AlreadyCancelled)]
    #[tokio::test]
    async fn cancel_an_already_latched_plan_is_a_reported_no_op(
        #[case] status: PlanStatus,
        #[case] expected: CancelOutcome,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, status)?;
        stores.plans.put_plan(&m).await?;

        let outcome = cancel_plan(stores.plans.as_ref(), &m.plan_id).await?;
        assert_eq!(outcome, expected);
        assert!(
            !stores.plans.has_cancel_marker(&m.plan_id).await?,
            "no marker written for an already-latched plan"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancel_twice_before_teardown_is_idempotent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, PlanStatus::Active)?;
        stores.plans.put_plan(&m).await?;

        for _ in 0..2 {
            let outcome = cancel_plan(stores.plans.as_ref(), &m.plan_id).await?;
            assert_eq!(outcome, CancelOutcome::Requested);
        }
        assert_eq!(
            stores.plans.list_cancel_markers().await?,
            vec![m.plan_id.clone()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancel_refuses_a_plan_with_no_manifest() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, PlanStatus::Active)?;

        let err = cancel_plan(stores.plans.as_ref(), &m.plan_id)
            .await
            .err()
            .context("cancel_plan must refuse a plan it cannot read")?;
        assert!(matches!(&err, PlanLookupError::NotFound { plan_id } if *plan_id == m.plan_id));
        assert_eq!(err.to_string(), format!("no plan {}", m.plan_id));
        assert!(stores.plans.list_cancel_markers().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn cancel_refuses_a_complete_plan_and_writes_no_marker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(1, PlanStatus::Complete)?;
        stores.plans.put_plan(&m).await?;

        let outcome = cancel_plan(stores.plans.as_ref(), &m.plan_id).await?;
        assert_eq!(outcome, CancelOutcome::AlreadyComplete);
        assert!(!stores.plans.has_cancel_marker(&m.plan_id).await?);
        assert!(stores.plans.list_cancel_markers().await?.is_empty());
        Ok(())
    }

    // ── explain_plan ────────────────────────────────────────────────────────

    /// The record and its `perform_work` grant.
    async fn enlist(stores: &Stores, client: &pipette_core::client::Client) -> anyhow::Result<()> {
        stores.clients.put_client(client).await?;
        stores
            .clients
            .put_client_permissions(
                &client.client_id,
                &pipette_core::client_permission::ClientPermissionSet::from_iter([
                    pipette_plan_types::ClientPermission::PerformWork,
                ]),
            )
            .await
    }

    /// An approved client running `os`, with one reported runtime flag.
    fn client(id: &str, os: &str) -> anyhow::Result<pipette_core::client::Client> {
        Ok(pipette_core::client::Client {
            client_id: ClientId::try_new(id)?,
            public_key: pipette_plan_types::PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: pipette_plan_types::NonEmptyTrimmedString::try_new("lab")?,
            client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("rig")?,
            contact_email: pipette_plan_types::ContactEmail::try_new("ops@example.com")?,
            status: ClientStatus::Approved,
            registered_at: "2026-01-01T00:00:00Z".parse()?,
            device_profile: pipette_plan_types::DeviceProfile {
                device_os_name: Some(pipette_plan_types::NonEmptyTrimmedString::try_new(os)?),
                ..Default::default()
            },
            capabilities: BTreeSet::from(["runtime:llama_cpp".to_string()]),
        })
    }

    /// Named so two bodies of one plan do not merge.
    fn job(name: &str, flag: &str) -> (String, Value) {
        (
            name.to_string(),
            json!({
                "requires": [flag],
                "spec": {"benchmark": "bench-1", "model": {"name": name}, "runtime": {}},
            }),
        )
    }

    async fn ingest(stores: &Stores, jobs: Vec<(String, Value)>) -> anyhow::Result<PlanId> {
        let catalog = catalog()?;
        Ok(ingest_jobs(
            &IngestContext::from_stores(
                stores,
                &catalog,
                ModelCheck::none(),
                MAX_JOBS,
                CREATING_MAX_AGE,
            ),
            PlanSubmission {
                note: None,
                change: None,
                submitted_by: None,
                membership: CampaignMembership::for_test(UserId::try_new("user_test")?),
                jobs,
                source: None,
                plan_warnings: Vec::new(),
            },
        )
        .await?
        .plan_id)
    }

    async fn run_maintenance(stores: &Stores) -> anyhow::Result<()> {
        crate::queue_maintenance::run(
            stores.todo.as_ref(),
            stores.clients.as_ref(),
            stores.tags.as_ref(),
            stores.submissions.as_ref(),
            stores.plans.as_ref(),
            &catalog()?,
            crate::queue_maintenance::MaintenanceSettings {
                tmp_max: CREATING_MAX_AGE,
                plan_creating_max: CREATING_MAX_AGE,
                lost_lease_limit: 3,
            },
        )
        .await
    }

    /// Replace a plan's staged job body under the `avail/` key its snapshot
    /// already names, so the cached group and the real body disagree.
    async fn overwrite_avail_body(
        stores: &Stores,
        plan_id: &PlanId,
        body: &Value,
    ) -> anyhow::Result<JobId> {
        let manifest = stores
            .plans
            .get_plan(plan_id)
            .await?
            .context("manifest written")?;
        let job_id = manifest.job_ids.first().context("one job")?;
        let expires_at = manifest.job_expiry.context("frozen expiry")?.earliest;
        stores.todo.write_tmp(job_id, body).await?;
        stores.todo.delete_avail(job_id, expires_at).await?;
        stores.todo.promote_avail(job_id, expires_at).await?;
        Ok(job_id.clone())
    }

    /// The store state a gate-chain case needs, applied after the ingest.
    #[derive(Debug, Clone, Copy)]
    enum Seed {
        JustIngested,
        /// One `queue-maintenance` pass.
        Indexed,
        IndexedThenSuspended,
        IndexedThenCancelled,
        IndexedThenDrained,
        IndexedThenLeased,
        IndexedThenLeaseLapsed,
    }

    async fn apply(stores: &Stores, plan_id: &PlanId, seed: Seed) -> anyhow::Result<()> {
        if matches!(seed, Seed::JustIngested) {
            return Ok(());
        }
        run_maintenance(stores).await?;
        match seed {
            Seed::IndexedThenSuspended => {
                stores
                    .todo
                    .write_suspension(
                        &ClientId::try_new("ev1_alpha")?,
                        Utc::now(),
                        &JobId::new_unchecked("job-stale"),
                    )
                    .await?
            }
            Seed::IndexedThenCancelled => stores.plans.write_cancel_marker(plan_id).await?,
            Seed::IndexedThenDrained => {
                let manifest = stores
                    .plans
                    .get_plan(plan_id)
                    .await?
                    .context("manifest written")?;
                for job_id in &manifest.job_ids {
                    stores.todo.delete_avail_by_job(job_id).await?;
                }
            }
            Seed::IndexedThenLeased | Seed::IndexedThenLeaseLapsed => {
                let hour = chrono::Duration::hours(1);
                let lease_expiry = if matches!(seed, Seed::IndexedThenLeaseLapsed) {
                    Utc::now() - hour
                } else {
                    Utc::now() + hour
                };
                lease_plan_jobs(
                    stores,
                    plan_id,
                    &ClientId::try_new("ev1_alpha")?,
                    lease_expiry,
                )
                .await?;
            }
            Seed::JustIngested | Seed::Indexed => {}
        }
        Ok(())
    }

    /// Every case runs against one approved `os:ios` client tagged `lab`.
    #[rstest]
    #[case::unindexed_and_matching(&["os:ios"], Seed::JustIngested, SchedulingState::NotIndexed)]
    #[case::unindexed_beats_starvation(&["os:android"], Seed::JustIngested, SchedulingState::NotIndexed)]
    #[case::schedulable(&["os:ios"], Seed::Indexed, SchedulingState::Schedulable)]
    #[case::starved(&["os:android"], Seed::Indexed, SchedulingState::Starved)]
    #[case::schedulable_by_tag(&["tag:lab"], Seed::Indexed, SchedulingState::Schedulable)]
    #[case::starved_on_a_tag(&["tag:bench"], Seed::Indexed, SchedulingState::Starved)]
    #[case::suspended_tagged_client_beats_starvation(&["tag:lab"], Seed::IndexedThenSuspended, SchedulingState::ClientsBlocked)]
    #[case::suspended_beats_starvation(&["os:ios"], Seed::IndexedThenSuspended, SchedulingState::ClientsBlocked)]
    #[case::cancel_pending(&["os:ios"], Seed::IndexedThenCancelled, SchedulingState::CancelPending)]
    #[case::nothing_outstanding(&["os:ios"], Seed::IndexedThenDrained, SchedulingState::NothingOutstanding)]
    // A live lease is work in flight, not a block.
    #[case::leased(&["os:ios"], Seed::IndexedThenLeased, SchedulingState::NothingOutstanding)]
    #[case::lease_lapsed(&["os:ios"], Seed::IndexedThenLeaseLapsed, SchedulingState::LeasesLapsed)]
    #[tokio::test]
    async fn explain_names_the_first_blocking_gate(
        #[case] requires: &[&str],
        #[case] seed: Seed,
        #[case] expected: SchedulingState,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let rig = client("ev1_alpha", "iOS")?;
        enlist(&stores, &rig).await?;
        stores
            .tags
            .add_client_tag(&rig.client_id, &pipette_plan_types::Tag::try_new("lab")?)
            .await?;
        let jobs = requires
            .iter()
            .enumerate()
            .map(|(i, flag)| job(&format!("job-{i}.json"), flag))
            .collect();
        let plan_id = ingest(&stores, jobs).await?;
        apply(&stores, &plan_id, seed).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(
            report.verdict,
            expected,
            "{}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    #[tokio::test]
    async fn explain_returns_the_status_view_plan_status_would() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        run_maintenance(&stores).await?;

        let alone = plan_status(stores.plans.as_ref(), &plan_id).await?;
        let (with_explain, _) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(alone, with_explain);
        Ok(())
    }

    /// A plan merged onto an indexed job clears the index gate with no
    /// snapshot, so only the frozen ingestion warnings name the starvation.
    #[tokio::test]
    async fn explain_reads_the_ingestion_warnings_when_no_snapshot_exists() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;

        let _first = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;

        let second = ingest(&stores, vec![job("a.json", "os:android")]).await?;

        let (_, report) = explain_plan(&stores, &second, Utc::now()).await?;
        assert_eq!(
            report.snapshot_computed_at, None,
            "no pass ran for this plan"
        );
        assert_eq!(report.census.indexed, 1, "merged onto an indexed job");
        assert_eq!(
            report.verdict,
            SchedulingState::Starved,
            "{}",
            report.headline(FleetDetail::Full)
        );
        assert!(report.groups.is_empty(), "no cached clause set to explain");
        assert!(
            report
                .headline(FleetDetail::Full)
                .contains("warnings that it froze"),
            "the headline names where the verdict came from: {}",
            report.headline(FleetDetail::Full)
        );
        assert!(
            report
                .headline(FleetDetail::Full)
                .contains("matched no working client"),
            "a frozen claim reads in the past tense, so an approval since the \
             ingest does not make the sentence false: {}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// The verdict names the milder gate; the strand shows in the census.
    #[tokio::test]
    async fn explain_reports_a_strand_beside_a_live_lease() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(
            &stores,
            vec![job("live.json", "os:ios"), job("strand.json", "os:ios")],
        )
        .await?;
        run_maintenance(&stores).await?;

        let hour = chrono::Duration::hours(1);
        let holder = ClientId::try_new("ev1_alpha")?;
        let mut keys = collect_all_avail(stores.todo.as_ref()).await?.keys;
        let stranded = keys.pop().context("two jobs minted")?;
        let held = keys.pop().context("two jobs minted")?;
        stores
            .todo
            .claim_job(
                &stranded.job_id,
                stranded.expires_at,
                &holder,
                Utc::now() - hour,
            )
            .await?;
        stores
            .todo
            .claim_job(&held.job_id, held.expires_at, &holder, Utc::now() + hour)
            .await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(report.verdict, SchedulingState::NothingOutstanding);
        assert_eq!(report.census.leased, 2);
        assert_eq!(report.census.lapsed_leases, 1);
        assert!(
            report
                .headline(FleetDetail::Full)
                .contains("1 behind a lapsed lease"),
            "headline names the strand: {}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// What blocks the one client that matches the plan's job.
    #[derive(Debug, Clone, Copy)]
    enum Blocker {
        Suspended,
        PendingReindex,
        Denied,
        None,
    }

    /// The unblocked `os:android` bystander is what a fleet-wide test would
    /// mistake for a taker.
    #[rstest]
    #[case::suspended_only_matcher(Blocker::Suspended, SchedulingState::ClientsBlocked)]
    #[case::reindexing_only_matcher(Blocker::PendingReindex, SchedulingState::ClientsBlocked)]
    #[case::denied_by_only_matcher(Blocker::Denied, SchedulingState::ClientsBlocked)]
    #[case::nothing_blocks_the_matcher(Blocker::None, SchedulingState::Schedulable)]
    #[tokio::test]
    async fn explain_weighs_each_job_against_the_clients_that_match_it(
        #[case] blocker: Blocker,
        #[case] expected: SchedulingState,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        enlist(&stores, &client("ev1_beta", "Android")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        run_maintenance(&stores).await?;

        let matcher = ClientId::try_new("ev1_alpha")?;
        let job_id = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .context("manifest written")?
            .job_ids
            .first()
            .cloned()
            .context("one job minted")?;
        match blocker {
            Blocker::Suspended => {
                stores
                    .todo
                    .write_suspension(&matcher, Utc::now(), &job_id)
                    .await?
            }
            Blocker::PendingReindex => stores.todo.write_pending_reindex(&matcher).await?,
            Blocker::Denied => {
                stores
                    .todo
                    .write_denied(&job_id, &matcher, &DenialRecord::default())
                    .await?
            }
            Blocker::None => {}
        }

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(
            report.verdict,
            expected,
            "{}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// An unreadable body proves the reads never happen.
    #[rstest]
    #[case::cancel_pending(Seed::IndexedThenCancelled, SchedulingState::CancelPending)]
    #[case::not_indexed(Seed::JustIngested, SchedulingState::NotIndexed)]
    #[tokio::test]
    async fn explain_answers_a_settled_gate_without_reading_a_body(
        #[case] seed: Seed,
        #[case] expected: SchedulingState,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        apply(&stores, &plan_id, seed).await?;

        // A directory where the body belongs, so any read of it fails.
        let key = collect_all_avail(stores.todo.as_ref())
            .await?
            .keys
            .first()
            .context("one job staged")?
            .key
            .clone();
        let body = dir.path().join("todo").join("avail").join(&key);
        std::fs::remove_file(&body)?;
        std::fs::create_dir(&body)?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(
            report.verdict,
            expected,
            "{}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// Why the job left `avail/` between the listing and the body read.
    #[derive(Debug, Clone, Copy)]
    enum Vanish {
        Claimed,
        ClaimedThenLapsed,
        Deleted,
    }

    /// The fleet matches nothing, so `takeable` can only come from the lease.
    #[rstest]
    #[case::claimed(Vanish::Claimed, Takeability::Takeable)]
    #[case::claimed_then_lapsed(Vanish::ClaimedThenLapsed, Takeability::Vanished)]
    #[case::deleted(Vanish::Deleted, Takeability::Vanished)]
    #[tokio::test]
    async fn scan_reads_a_claim_as_evidence_the_plan_moves(
        #[case] vanish: Vanish,
        #[case] expected: Takeability,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let bystander = client("ev1_beta", "Android")?;
        enlist(&stores, &bystander).await?;
        ingest(&stores, vec![job("a.json", "os:ios")]).await?;

        let listing = collect_all_avail(stores.todo.as_ref()).await?;
        let key = listing.keys.first().context("one job staged")?;

        match vanish {
            Vanish::Claimed | Vanish::ClaimedThenLapsed => {
                let lease_expiry = if matches!(vanish, Vanish::ClaimedThenLapsed) {
                    Utc::now() - chrono::Duration::minutes(5)
                } else {
                    Utc::now() + chrono::Duration::minutes(5)
                };
                stores
                    .todo
                    .claim_job(
                        &key.job_id,
                        key.expires_at,
                        &bystander.client_id,
                        lease_expiry,
                    )
                    .await?;
            }
            Vanish::Deleted => stores.todo.delete_avail_by_job(&key.job_id).await?,
        }

        let fleet = [&bystander];
        let takeability = scan_takeability(
            stores.todo.as_ref(),
            &[key],
            &HashSet::new(),
            ScanFleet {
                working: &fleet,
                unblocked: &fleet,
                denied: &HashMap::new(),
                tags: &matching::FleetTags::default(),
            },
            &mut HashMap::new(),
            Utc::now(),
        )
        .await?;
        assert_eq!(takeability, expected);
        Ok(())
    }

    /// The cached groups explain only a subset when the live scan reaches
    /// `starved`, and the headline must say so.
    #[tokio::test]
    async fn explain_says_which_starved_jobs_the_snapshot_accounts_for() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        // Keeps the unblocked set non-empty without matching either job.
        enlist(&stores, &client("ev1_beta", "Linux")?).await?;
        let plan_id = ingest(
            &stores,
            vec![job("a.json", "os:android"), job("b.json", "os:ios")],
        )
        .await?;
        run_maintenance(&stores).await?;

        let snapshot = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .context("manifest written")?
            .progress_snapshot
            .context("a pass computed the snapshot")?;
        assert_eq!(
            snapshot.starved.len(),
            1,
            "the premise: one job starves and one does not"
        );

        // The second job's only matcher loses its approval, with no pass since.
        let mut alpha = client("ev1_alpha", "iOS")?;
        alpha.status = ClientStatus::Pending;
        enlist(&stores, &alpha).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(
            report.verdict,
            SchedulingState::Starved,
            "{}",
            report.headline(FleetDetail::Full)
        );
        assert!(report.starved_by_live_scan);
        assert!(
            report
                .headline(FleetDetail::Full)
                .contains("name only some of them"),
            "the headline does not offer one group as the whole account: {}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// Lifting the suspension would still leave a job no client matches, so the
    /// clause is the answer that helps.
    #[tokio::test]
    async fn explain_reports_starvation_behind_a_fully_suspended_fleet() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;
        stores
            .todo
            .write_suspension(
                &ClientId::try_new("ev1_alpha")?,
                Utc::now(),
                &JobId::new_unchecked("job-stale"),
            )
            .await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(
            report.fleet.unblocked, 0,
            "the premise: nobody can take work"
        );
        assert_eq!(
            report.verdict,
            SchedulingState::Starved,
            "{}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// `clients blocked` would send the operator to audit client standing that
    /// is in good order; the headline names the stale snapshot instead.
    #[tokio::test]
    async fn explain_reports_starvation_the_stale_snapshot_missed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        // The bystander keeps the unblocked set non-empty, so the scan runs.
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        enlist(&stores, &client("ev1_beta", "Android")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        run_maintenance(&stores).await?;

        let snapshot = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .context("manifest written")?
            .progress_snapshot
            .context("a pass computed the snapshot")?;
        assert!(
            snapshot.starved.is_empty(),
            "the premise: the snapshot calls the plan healthy"
        );

        // The only matching client loses its approval, with no pass since.
        let mut alpha = client("ev1_alpha", "iOS")?;
        alpha.status = ClientStatus::Pending;
        enlist(&stores, &alpha).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(
            report.verdict,
            SchedulingState::Starved,
            "{}",
            report.headline(FleetDetail::Full)
        );
        assert!(report.starved_by_live_scan);
        assert!(
            report
                .headline(FleetDetail::Full)
                .contains("snapshot is stale or absent"),
            "the headline sends the operator to `queue-maintenance`: {}",
            report.headline(FleetDetail::Full)
        );
        Ok(())
    }

    /// A plan that reads `active` and runs nothing: the report contradicts it.
    #[tokio::test]
    async fn explain_reports_an_unindexed_queue_under_an_active_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(
            &stores,
            vec![job("a.json", "os:ios"), job("b.json", "os:ios")],
        )
        .await?;

        let manifest = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .context("manifest written")?;
        assert_eq!(manifest.status, PlanStatus::Active, "the misleading status");
        assert!(manifest.progress_snapshot.is_none());

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(report.verdict, SchedulingState::NotIndexed);
        assert_eq!(report.snapshot_computed_at, None, "gate 0 leads the report");
        assert_eq!(report.census.claimable, 2);
        assert_eq!(report.census.indexed, 0);
        assert!(report.groups.is_empty(), "no snapshot, so no starved group");
        assert_eq!(report.census.available, 2);
        assert_eq!(report.fleet.approved, 1);
        Ok(())
    }

    /// The group also counts registered-but-unapproved matches, which an
    /// approval fixes.
    #[tokio::test]
    async fn explain_narrows_a_starved_group_to_the_blocking_clause() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let mut pending = client("ev1_beta", "Android")?;
        pending.status = ClientStatus::Pending;
        enlist(&stores, &pending).await?;

        let plan_id = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(report.verdict, SchedulingState::Starved);
        let group = report.groups.first().context("one starved group")?;
        assert_eq!(group.job_count, 1);
        assert_eq!(
            group
                .ladder
                .iter()
                .map(|rung| (rung.clause.as_str(), rung.surviving_client_count))
                .collect::<Vec<_>>(),
            vec![("requires os:android", 0)]
        );
        assert_eq!(group.never_matches, None, "the clauses are satisfiable");
        assert_eq!(group.roster, None, "the group names no client id");
        assert_eq!(group.live_approved_client_count, 0);
        assert_eq!(
            group.live_registered_client_count, 1,
            "an approval clears this group"
        );
        Ok(())
    }

    #[tokio::test]
    async fn explain_reports_a_live_match_against_a_stale_group() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;
        enlist(&stores, &client("ev1_droid", "Android")?).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        let group = report.groups.first().context("the group is still cached")?;
        assert_eq!(group.live_approved_client_count, 1);
        assert_eq!(
            group
                .ladder
                .iter()
                .map(|rung| rung.surviving_client_count)
                .collect::<Vec<_>>(),
            vec![1],
            "the ladder counts the live fleet"
        );
        // The verdict reads the snapshot, so the headline dates its count.
        assert_eq!(report.verdict, SchedulingState::Starved);
        let headline = report.headline(FleetDetail::Full);
        assert!(
            headline.contains("matched no working client when the snapshot was computed"),
            "{headline}"
        );
        assert!(
            headline.contains("Every group matches a working client now"),
            "{headline}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn explain_counts_a_newly_tagged_client_as_a_live_match() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let rig = client("ev1_alpha", "iOS")?;
        enlist(&stores, &rig).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "tag:lab")]).await?;
        run_maintenance(&stores).await?;

        let ladder = |report: &PlanExplanation| -> anyhow::Result<Vec<(String, usize)>> {
            Ok(report
                .groups
                .first()
                .context("one starved group")?
                .ladder
                .iter()
                .map(|rung| (rung.clause.clone(), rung.surviving_client_count))
                .collect())
        };
        let (_, untagged) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(untagged.verdict, SchedulingState::Starved);
        assert_eq!(
            ladder(&untagged)?,
            vec![("requires tag:lab".to_string(), 0)]
        );

        stores
            .tags
            .add_client_tag(&rig.client_id, &pipette_plan_types::Tag::try_new("lab")?)
            .await?;

        let (_, tagged) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(ladder(&tagged)?, vec![("requires tag:lab".to_string(), 1)]);
        let group = tagged.groups.first().context("the group is still cached")?;
        assert_eq!(group.live_approved_client_count, 1);
        Ok(())
    }

    /// An approved client that holds no `perform_work` (as registration leaves
    /// it) is not a live match, so the note names a grant, not a clause.
    #[tokio::test]
    async fn explain_names_the_missing_perform_work_grant() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;
        stores
            .clients
            .put_client(&client("ev1_droid", "Android")?)
            .await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(report.fleet.approved, 2);
        assert_eq!(
            report.fleet.working, 1,
            "only the granted client can be offered work"
        );
        let group = report.groups.first().context("the group is still cached")?;
        assert_eq!(
            (
                group.live_registered_client_count,
                group.live_approved_client_count
            ),
            (1, 1),
            "the Android device is registered and approved"
        );
        assert_eq!(
            group.live_working_client_count, 0,
            "and the claim path would refuse it"
        );
        assert_eq!(
            group
                .ladder
                .iter()
                .map(|rung| rung.surviving_client_count)
                .collect::<Vec<_>>(),
            vec![0],
            "the ladder narrows the working fleet, so the clause blames nobody"
        );
        // Not "a `queue-maintenance` run clears it": only the grant does.
        assert_eq!(report.verdict, SchedulingState::Starved);
        let headline = report.headline(FleetDetail::Full);
        assert!(headline.contains("match no working client"), "{headline}");
        Ok(())
    }

    /// A body that can never match names the rule and prints no ladder. The
    /// malformed case pins reading one real body rather than the lenient group.
    #[rstest]
    #[case::malformed_requires(json!({"requires": "os:android"}), "not a well-formed array")]
    #[case::contradictory_clauses(
        json!({"requires": ["os:android"], "excludes": ["os:android"]}),
        "`excludes` names a flag `requires` demands"
    )]
    #[tokio::test]
    async fn explain_reads_a_body_for_the_shape_verdict(
        #[case] bad_body: Value,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;

        overwrite_avail_body(&stores, &plan_id, &bad_body).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        let group = report.groups.first().context("one starved group")?;
        let rule = group.never_matches.as_deref().context("a proven rule")?;
        assert!(rule.contains(expected), "{rule}");
        assert!(
            !rule.contains("`requires` is empty"),
            "the lenient group read would say this: {rule}"
        );
        assert!(
            group.ladder.is_empty(),
            "a body that can never match needs no ladder"
        );
        Ok(())
    }

    /// A rule proving nobody matches never sits beside a count of clients that
    /// do: needs a well-formed cached group, a malformed body, and a stale fleet.
    #[tokio::test]
    async fn explain_counts_live_matches_against_the_real_body() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:android")]).await?;
        run_maintenance(&stores).await?;
        // Registers after the pass, so the cached clause set matches one client.
        enlist(&stores, &client("ev1_droid", "Android")?).await?;

        overwrite_avail_body(
            &stores,
            &plan_id,
            &json!({"requires": ["os:android"], "excludes": "chip:a15"}),
        )
        .await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        let group = report.groups.first().context("one starved group")?;
        assert!(
            group.never_matches.is_some(),
            "the real body is malformed, so a rule settles the group"
        );
        assert_eq!(
            group.live_approved_client_count, 0,
            "a malformed body matches nobody, whatever the cached clauses hold"
        );
        assert_eq!(group.live_registered_client_count, 0);
        Ok(())
    }

    /// With no `requires`, a capability ladder would blame the wrong clause.
    #[tokio::test]
    async fn explain_reports_roster_standing_for_a_pinned_group() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(
            &stores,
            vec![(
                "pinned.json".to_string(),
                json!({
                    "clients": ["ev1_ghost"],
                    "spec": {"benchmark": "bench-1", "model": {"name": "pinned"}, "runtime": {}},
                }),
            )],
        )
        .await?;
        run_maintenance(&stores).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(report.verdict, SchedulingState::Starved);
        let group = report.groups.first().context("one starved group")?;
        assert!(group.ladder.is_empty(), "no `requires`, so no ladder");
        let roster = group.roster.context("the group names a client id")?;
        assert_eq!(roster.listed, 1);
        assert_eq!(roster.registered, 0, "nobody has registered `ev1_ghost`");
        assert_eq!(roster.approved, 0);
        assert_eq!(roster.eligible, 0);
        Ok(())
    }

    /// A roster's standing reads tags too: an excluded tag denies a listed,
    /// working client, and the drop shows at `eligible`.
    #[tokio::test]
    async fn explain_denies_a_roster_client_an_excluded_tag_names() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let rig = client("ev1_alpha", "iOS")?;
        enlist(&stores, &rig).await?;
        stores
            .tags
            .add_client_tag(&rig.client_id, &pipette_plan_types::Tag::try_new("busy")?)
            .await?;
        let plan_id = ingest(
            &stores,
            vec![(
                "pinned.json".to_string(),
                json!({
                    "clients": ["ev1_alpha"],
                    "excludes": ["tag:busy"],
                    "spec": {"benchmark": "bench-1", "model": {"name": "pinned"}, "runtime": {}},
                }),
            )],
        )
        .await?;
        run_maintenance(&stores).await?;

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        let group = report.groups.first().context("one starved group")?;
        let roster = group.roster.context("the group names a client id")?;
        assert_eq!((roster.working, roster.eligible), (1, 0));
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum Tree {
        Avail,
        Leased,
    }

    /// The key disturbs no bucket: "0 of N indexed" must not quietly mean "0 of
    /// the N I parsed".
    #[rstest]
    #[case::avail(Tree::Avail)]
    #[case::leased(Tree::Leased)]
    #[tokio::test]
    async fn explain_carries_the_unparseable_key_count(#[case] tree: Tree) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        match tree {
            Tree::Avail => std::fs::write(dir.path().join("todo/avail/not-a-job-key.json"), b"{}")?,
            Tree::Leased => {
                let partition = dir.path().join("todo/leased/ev1_alpha");
                std::fs::create_dir_all(&partition)?;
                std::fs::write(partition.join("not-a-lease.json"), b"{}")?;
            }
        }

        let (_, report) = explain_plan(&stores, &plan_id, Utc::now()).await?;
        let c = &report.census;
        assert_eq!(c.total, 1);
        match tree {
            Tree::Avail => {
                assert_eq!(c.unparseable_avail_keys, 1);
                assert_eq!(c.unparseable_leased_keys, 0);
            }
            Tree::Leased => {
                assert_eq!(c.unparseable_leased_keys, 1);
                assert_eq!(c.unparseable_avail_keys, 0);
                assert_eq!(c.leased, 0, "the bad key joins no bucket");
            }
        }
        assert_eq!(c.available, 1, "the plan's own job alone");
        Ok(())
    }

    #[tokio::test]
    async fn explain_reports_an_expired_queue() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_alpha", "iOS")?).await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;

        // Report from past the frozen deadline rather than re-dating the queue.
        let manifest = stores
            .plans
            .get_plan(&plan_id)
            .await?
            .context("manifest written")?;
        let latest = match manifest.job_expiry.context("frozen expiry")?.latest {
            pipette_plan_types::ExpiresAt::At(at) => at,
            pipette_plan_types::ExpiresAt::Never => {
                anyhow::bail!("the default deadline is bounded")
            }
        };

        let (_, report) = explain_plan(&stores, &plan_id, latest + Duration::seconds(1)).await?;
        assert_eq!(report.verdict, SchedulingState::Expired);
        assert_eq!(report.census.available, 1);
        assert_eq!(report.census.expired, 1);
        assert_eq!(report.census.claimable, 0);
        Ok(())
    }

    #[tokio::test]
    async fn explain_reports_a_creating_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let m = manifest(7, PlanStatus::Creating)?;
        stores.plans.put_plan(&m).await?;

        let (_, report) = explain_plan(&stores, &m.plan_id, Utc::now()).await?;
        assert_eq!(report.verdict, SchedulingState::StillCreating);
        assert_eq!(report.census.total, 1);
        assert_eq!(report.census.available, 0);
        Ok(())
    }
    // ── plan_leases ─────────────────────────────────────────────────────────

    /// Lease `plan_id`'s jobs to `client_id`; a past `lease_expiry` seeds a strand.
    async fn lease_plan_jobs(
        stores: &Stores,
        plan_id: &PlanId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let manifest = stores
            .plans
            .get_plan(plan_id)
            .await?
            .context("manifest written")?;
        let members: HashSet<JobId> = manifest.job_ids.iter().cloned().collect();
        for key in collect_all_avail(stores.todo.as_ref())
            .await?
            .keys
            .into_iter()
            .filter(|key| members.contains(&key.job_id))
        {
            stores
                .todo
                .claim_job(&key.job_id, key.expires_at, client_id, lease_expiry)
                .await?;
        }
        Ok(())
    }

    #[rstest]
    #[case::nothing_held(Seed::Indexed, 0, 0)]
    #[case::live_hold(Seed::IndexedThenLeased, 1, 0)]
    #[case::lapsed_hold(Seed::IndexedThenLeaseLapsed, 0, 1)]
    #[tokio::test]
    async fn plan_leases_splits_live_holds_from_stranded_jobs(
        #[case] seed: Seed,
        #[case] live: usize,
        #[case] lapsed: usize,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .clients
            .put_client(&client("ev1_alpha", "iOS")?)
            .await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        apply(&stores, &plan_id, seed).await?;

        let (_, held) = plan_leases(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(held.live.len(), live, "live holders");
        assert_eq!(held.lapsed.len(), lapsed, "stranded jobs");
        assert_eq!(held.unparseable_keys, 0);
        assert_eq!(held.is_empty(), live == 0 && lapsed == 0);
        held.live.iter().chain(&held.lapsed).for_each(|lease| {
            assert_eq!(
                lease.client_id.as_str(),
                "ev1_alpha",
                "the row names the client that took the job"
            );
        });
        Ok(())
    }

    #[tokio::test]
    async fn plan_leases_ignores_a_hold_on_a_job_outside_the_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .clients
            .put_client(&client("ev1_alpha", "iOS")?)
            .await?;
        let holder = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        let other = ingest(&stores, vec![job("b.json", "os:ios")]).await?;
        run_maintenance(&stores).await?;
        lease_plan_jobs(
            &stores,
            &holder,
            &ClientId::try_new("ev1_alpha")?,
            Utc::now() + Duration::hours(1),
        )
        .await?;

        let (_, mine) = plan_leases(&stores, &holder, Utc::now()).await?;
        assert_eq!(mine.live.len(), 1, "the plan whose job is held reports it");
        let (_, theirs) = plan_leases(&stores, &other, Utc::now()).await?;
        assert!(
            theirs.is_empty(),
            "another plan's hold is not this plan's work"
        );
        Ok(())
    }

    #[tokio::test]
    async fn plan_leases_carries_the_unparseable_key_count() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .clients
            .put_client(&client("ev1_alpha", "iOS")?)
            .await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        let partition = dir.path().join("todo/leased/ev1_alpha");
        std::fs::create_dir_all(&partition)?;
        std::fs::write(partition.join("not-a-lease.json"), b"{}")?;

        let (_, held) = plan_leases(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(held.unparseable_keys, 1);
        assert!(held.is_empty(), "the bad key joins neither list");
        Ok(())
    }

    #[tokio::test]
    async fn plan_leases_returns_the_status_view_plan_status_would() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        stores
            .clients
            .put_client(&client("ev1_alpha", "iOS")?)
            .await?;
        let plan_id = ingest(&stores, vec![job("a.json", "os:ios")]).await?;
        run_maintenance(&stores).await?;

        let alone = plan_status(stores.plans.as_ref(), &plan_id).await?;
        let (with_leases, _) = plan_leases(&stores, &plan_id, Utc::now()).await?;
        assert_eq!(alone, with_leases);
        Ok(())
    }
}
