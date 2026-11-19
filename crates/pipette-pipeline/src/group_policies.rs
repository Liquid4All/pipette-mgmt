//! Campaign group policies: the four checks that re-run and mark, read as the
//! console reads them, and the model params and smaller failed checks, which
//! only flag; what the policies would re-run and mark in one member campaign's
//! current plan, job by job, as its results land, the revision that files it,
//! the queue-maintenance pass that does so for every group and records how each
//! re-run answers, and the backfill of the re-runs filed before it recorded
//! them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::{StreamExt, TryStreamExt};
use jiff::Timestamp;
use serde_json::Value;
use sha2::{Digest, Sha256};
use strum::IntoEnumIterator as _;
use tokio::sync::OnceCell;

use pipette_core::BUILD_VERSION;
use pipette_core::audit::new_event;
use pipette_core::benchmark::Benchmark;
use pipette_core::campaign::{
    Campaign, CampaignGroup, GroupFiling, GroupPolicyState, JobPolicyState, RecordedModel, Refusal,
};
use pipette_core::config::Config;
use pipette_core::plan::{PlanManifest, PlanSummary};
use pipette_plan_types::{
    AuditAction, AuditOutcome, AuditTarget, BenchmarkId, CampaignGroupId, CampaignGroupPolicy,
    CampaignId, ExpiresAt, FailureCode, FailureSource, JobId, MAX_RERUNS, ModelParamsGap,
    NonEmptyTrimmedString, PlanId, PlanStatus, PolicyCheckCounts, PolicyKind, PolicyMemberState,
    PolicyRerun, PolicySettings, PolicySkipReason, RerunOutcome, RevisionChange, SmallerFailure,
};
use pipette_store::{
    CampaignGroupStore, GroupMember, JobHolder, JobHolderListing, JobStateChanges, PlanStore,
    STORAGE_CONCURRENCY, Stores, SubmissionStore, TodoStore,
};

use crate::audit::{AuditReach, host_actor};
use crate::campaign_admin::{self, CampaignAdminError, Head, Revised};
use crate::model_params::{ModelParamTable, descriptor_key};
use crate::plan_cli::{self, PlanInput};
use crate::plan_document::DocumentError;
use crate::plan_ingestion::{IngestContext, IngestError, IngestSettings, ModelCheck, size_family};
use crate::plan_results::{ResultRow, ResultsDb, ResultsError, rows_by_job};
use crate::queue_maintenance::millis;
use crate::score::{VaultNames, policy_rows};
use crate::submission::parse_stored_submission;

/// The runs of a re-run chain, the job included, at which no policy re-runs
/// the job: one flagged on every run spends `MAX_RERUNS` there anyway.
const CHAIN_RUNS: u32 = MAX_RERUNS + 1;

/// The version of what the policies do with a job they flag, which a member's
/// [`GroupPolicyState::rules`] records. Bump it with a change to those rules,
/// so the pass reads every answered job once under the new ones, as after a
/// policy save. 1: spread marks a cell failed once its re-runs are spent.
const RULES: u32 = 1;

/// A member campaign read for its group's policies.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    /// The plan the campaign runs.
    pub plan_id: Option<PlanId>,
    pub state: PolicyMemberState,
    /// One per kind, in kind order. The policy pass counts only the policies
    /// that are on.
    pub checks: Vec<PolicyCheckCounts>,
    /// The plan's jobs with a recorded outcome, as its `settled_jobs` holds
    /// them, an eval's left out ([`performance_jobs`]).
    pub answered_job_count: usize,
    /// Answered jobs with no rows in pipette-duckdb yet, except the jobs of
    /// `from_body`.
    pub waiting_job_count: usize,
    /// The jobs the policies that are on would re-run together.
    pub rerun_job_ids: BTreeSet<JobId>,
    /// The jobs of `rerun_job_ids` that the policies flag on rows built from
    /// their submission bodies, as pipette-duckdb has no rows for them yet.
    pub from_body: BTreeSet<JobId>,
    /// Submission-body lookups, including the second read of each re-run
    /// candidate, when [`Pass::bodies`] is given.
    pub body_reads: usize,
    /// The jobs to mark failed, each with its reason.
    pub marked: BTreeMap<JobId, String>,
    /// Answered jobs with rows that stay as they are: no policy that is on
    /// flags them, or the ones that do have no re-run left and do not mark.
    pub kept_job_count: usize,
    pub skipped: BTreeMap<JobId, PolicySkipReason>,
    /// The earliest deadline of the plan's jobs. The ingest refuses a re-run
    /// of a document that sets one that has passed.
    pub earliest_deadline: Option<ExpiresAt>,
    /// The policies that are on that re-run or mark a job: the ones a filing
    /// names.
    pub kinds: BTreeSet<PolicyKind>,
    /// What a filing records of each job of `rerun_job_ids`: one entry per
    /// policy that re-runs it.
    pub reruns: BTreeMap<JobId, Vec<Rerunning>>,
    /// The re-runs of [`Pass::waiting`] that this read gave an outcome: the
    /// ones whose rows it read, and the ones dropped.
    pub judged: Vec<PolicyRerun>,
    /// The rows read that lack model params, by the key an entry would take,
    /// in key order; found only when [`Pass::table`] is given.
    pub missing_model_params: Vec<ModelParamsGap>,
    /// The failed jobs read whose family holds a larger job with rows and no
    /// failure; found only when [`Pass::catalog`] is given.
    pub smaller_failed: Vec<SmallerFailure>,
    /// The jobs this read decided: kept, or left for good.
    pub decided: BTreeSet<JobId>,
    /// The checks of the policies that are on that flag each job this read
    /// walked.
    pub flagged: BTreeMap<JobId, BTreeSet<PolicyKind>>,
    /// A job state for each job with rows that this read read, smaller_failed
    /// aside, which needs every job's.
    pub read: Vec<JobPolicyState>,
    /// The member's job states once the policy pass read its rows: those it
    /// read and those decided before, in job id order. `None` leaves the
    /// stored ones as they are.
    pub job_states: Option<Vec<JobPolicyState>>,
    /// What holds the member before its rows are read, for the states
    /// `waiting_for_devices`, `filing_not_advanced` and `refused`.
    pub hold: Option<Hold>,
    /// What the policy pass keeps of a member whose rows it read, or failed to
    /// read, which [`file()`] completes with its filing, or of one its queued
    /// re-runs or a refusal holds; `None` from the preview and for a member not
    /// read.
    pub record: Option<GroupPolicyState>,
}

/// Why the policy pass reads no rows of a member this round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// `waiting_for_devices`: `waiting` jobs that the last group filing,
    /// `plan_id`, queued wait in the queue with no lease.
    Queued { plan_id: PlanId, waiting: usize },
    /// `filing_not_advanced`: the last group filing, `plan_id`, still runs
    /// and the campaign does not, whatever the plan it runs reads. The pass
    /// cancels it, unless it ended in `CommitUnknown`, whose head write can
    /// still land, so only an operator may cancel it.
    NotAdvanced {
        plan_id: PlanId,
        commit_unknown: bool,
    },
    /// `refused`: why the head cannot be filed again.
    Refused(String),
}

impl Hold {
    /// The state of a member this holds.
    fn state(&self) -> PolicyMemberState {
        match self {
            Self::Queued { .. } => PolicyMemberState::WaitingForDevices,
            Self::NotAdvanced { .. } => PolicyMemberState::FilingNotAdvanced,
            Self::Refused(_) => PolicyMemberState::Refused,
        }
    }
}

/// What the policy pass, or the preview, brings to a member's evaluation: what
/// the pass kept of the member, the default where it has written nothing, the
/// queue that the jobs of the member's last group filing wait in, and the
/// recorded results.
pub struct Pass<'a> {
    pub state: &'a GroupPolicyState,
    pub todo: &'a dyn TodoStore,
    pub submissions: &'a dyn SubmissionStore,
    /// The preview, which takes from `state` only what holds the member.
    pub preview: bool,
    /// The model params table the model_params check reads; `None` skips it.
    pub table: Option<&'a ModelParamTable>,
    /// The benchmark catalog the smaller_failed check reads its jobs' sizes
    /// by; `None` skips it. It reads every answered job's rows, which the
    /// policy pass does not, so only the preview gives it.
    pub catalog: Option<&'a HashMap<BenchmarkId, Benchmark>>,
    /// The member's recorded re-runs that wait for their rows.
    pub waiting: &'a [PolicyRerun],
    /// The member's stored job states; the preview takes none.
    pub job_states: &'a [JobPolicyState],
    /// What the pass reads the submission bodies of answered jobs with no
    /// rows with; `None` reads pipette-duckdb rows alone, as the preview does.
    pub bodies: Option<BodyReads<'a>>,
}

/// What builds rows from a submission body: the benchmark catalog, and the
/// stores that the scorer reads a body's vault names from.
#[derive(Clone, Copy)]
pub struct BodyReads<'a> {
    pub catalog: &'a HashMap<BenchmarkId, Benchmark>,
    pub stores: &'a Stores,
}

/// Read `campaign`, a member of `group`, under the group's `stored` policies
/// (a kind not stored reads as its defaults): what the policies that are on
/// would re-run and mark in its current plan, and what each would do alone.
/// Only the answered jobs of performance benchmarks are read; no policy judges
/// an eval's ([`performance_jobs`]).
/// `results` is `None` once an earlier member's pipette-duckdb read failed:
/// a member read that far then reads [`PolicyMemberState::Unreadable`], as
/// does one whose own read fails or times out. A member whose last group
/// filing has not been taken up, or whose head was refused under this build
/// and the same policies that re-run or mark, is held; the preview still reads
/// the rows of one that waits for devices or was refused, for the model params
/// check. The policy pass leaves out the jobs decided under the same policies
/// and counts only the policies that are on; the preview reads every answered
/// job and counts each policy, on or off. With [`Pass::bodies`], the rows of
/// an answered job with none in pipette-duckdb come from its submission body,
/// and they act only by a re-run. It writes nothing: the policy pass
/// cancels the filing a [`Hold::NotAdvanced`] names. Failed plan-store reads
/// return errors. Failed submission-body or vault-name reads leave jobs
/// waiting for result rows.
pub async fn evaluate(
    plans: &dyn PlanStore,
    results: Option<&ResultsDb>,
    group: &CampaignGroup,
    campaign: &Campaign,
    stored: &[CampaignGroupPolicy],
    pass: Pass<'_>,
) -> anyhow::Result<Evaluation> {
    let policies: Vec<CampaignGroupPolicy> = PolicyKind::iter()
        .map(|kind| kind.policy(&group.group_id, stored))
        .collect();
    let evaluation = Evaluation {
        plan_id: campaign.current_plan_id.clone(),
        state: PolicyMemberState::Evaluated,
        checks: policies
            .iter()
            .map(|policy| PolicyCheckCounts {
                kind: policy.kind,
                on: policy.enabled,
                flagged: 0,
                rerun: 0,
                marked: 0,
                kept: 0,
            })
            .collect(),
        answered_job_count: 0,
        waiting_job_count: 0,
        rerun_job_ids: BTreeSet::new(),
        from_body: BTreeSet::new(),
        body_reads: 0,
        marked: BTreeMap::new(),
        kept_job_count: 0,
        skipped: BTreeMap::new(),
        earliest_deadline: None,
        kinds: BTreeSet::new(),
        reruns: BTreeMap::new(),
        judged: Vec::new(),
        missing_model_params: Vec::new(),
        smaller_failed: Vec::new(),
        decided: BTreeSet::new(),
        flagged: BTreeMap::new(),
        read: Vec::new(),
        job_states: None,
        hold: None,
        record: None,
    };
    if let Some(hold) = unadvanced(plans, pass.state, campaign).await? {
        return Ok(Evaluation {
            state: hold.state(),
            hold: Some(hold),
            ..evaluation
        });
    }
    let head = match head(plans, group, campaign).await? {
        Ok(head) => head,
        Err(state) => {
            return Ok(Evaluation {
                state,
                ..evaluation
            });
        }
    };
    let sources = Sources::new(plans, head.settled_jobs.keys().cloned().collect());
    let settled = performance_jobs(&head, &sources).await?;
    let read = Evaluation {
        answered_job_count: settled.len(),
        earliest_deadline: head.job_expiry.map(|expiry| expiry.earliest),
        ..evaluation
    };
    let Some(results) = results else {
        return Ok(Evaluation {
            state: PolicyMemberState::Unreadable,
            ..read
        });
    };
    let acting = acting(&group.group_id, stored);
    if let Some(hold) = held(plans, &pass, &head, &acting).await? {
        let held = Evaluation {
            state: hold.state(),
            hold: Some(hold),
            ..read
        };
        if !pass.preview {
            // Its rows are not read, so its waiting re-runs and kept job states tell.
            let jobs: HashSet<&JobId> = head.job_ids.iter().collect();
            let kept: Vec<JobPolicyState> = pass
                .job_states
                .iter()
                .filter(|job| jobs.contains(&job.job_id))
                .cloned()
                .collect();
            let record = GroupPolicyState {
                triggered: triggered(&acting, &kept, pass.waiting, &[]),
                ..pass.state.clone()
            };
            return Ok(Evaluation {
                record: Some(record),
                ..held
            });
        }
        let Some(table) = pass.table else {
            return Ok(held);
        };
        let families = families(&sources, pass.catalog, &campaign.campaign_id, &settled).await?;
        drop(sources);
        return Ok(match results.all_job_results(&settled).await {
            Ok(rows) => {
                let rows = rows_by_job(rows);
                with_smaller_failed(families.as_ref(), &rows, with_params(table, &rows, held))
            }
            Err(error) => {
                unanswered(&campaign.campaign_id, error);
                Evaluation {
                    state: PolicyMemberState::Unreadable,
                    hold: None,
                    ..held
                }
            }
        });
    }
    // The job states decided under the same policies and rules on jobs the head
    // still holds.
    let decided: Vec<&JobPolicyState> =
        if !pass.preview && pass.state.policies == acting && pass.state.rules == RULES {
            let jobs: HashSet<&JobId> = head.job_ids.iter().collect();
            pass.job_states
                .iter()
                .filter(|job_state| job_state.decided && jobs.contains(&job_state.job_id))
                .collect()
        } else {
            Vec::new()
        };
    let done: HashSet<&JobId> = decided.iter().map(|job_state| &job_state.job_id).collect();
    let answered: Vec<JobId> = settled
        .into_iter()
        .filter(|job_id| !done.contains(job_id))
        .collect();
    // The pass walks no chain and reads no rows for a policy that is off.
    let tallied = if pass.preview { &policies } else { &acting };
    let unrecorded = unrecorded(pass.submissions, &head, pass.waiting).await?;
    let families = families(&sources, pass.catalog, &campaign.campaign_id, &answered).await?;
    // The bodies are not needed past the families, so they are not held through
    // the result rows that `judge` reads.
    drop(sources);
    let judged = judge(
        plans,
        results,
        &group.group_id,
        &campaign.campaign_id,
        &head,
        tallied,
        pass.table,
        families.as_ref(),
        &answered,
        pass.waiting,
        &unrecorded,
        pass.bodies.map(|bodies| (pass.submissions, bodies)),
        read.clone(),
    )
    .await;
    match judged {
        Ok(evaluation) if pass.preview => Ok(evaluation),
        Ok(evaluation) => {
            let job_states = job_states(&acting, pass.table, decided, &evaluation.read);
            let triggered = triggered(&acting, &job_states, pass.waiting, &evaluation.judged);
            let record = GroupPolicyState {
                policies: acting,
                triggered,
                filed: pass.state.filed.clone(),
                refusal: None,
                unreadable_at: None,
                rules: RULES,
            };
            Ok(Evaluation {
                record: Some(record),
                job_states: Some(job_states),
                ..evaluation
            })
        }
        Err(error) => {
            unanswered(&campaign.campaign_id, error);
            let record = (!pass.preview).then(|| GroupPolicyState {
                unreadable_at: Some(Timestamp::now()),
                ..pass.state.clone()
            });
            Ok(Evaluation {
                state: PolicyMemberState::Unreadable,
                record,
                ..read
            })
        }
    }
}

/// The member's job states after a read: `decided`, with the models of their
/// rows checked against `table` again, and `read`, with model_params' and
/// smaller_failed's flags taken afresh over all of them, in job id order.
fn job_states(
    acting: &[CampaignGroupPolicy],
    table: Option<&ModelParamTable>,
    decided: Vec<&JobPolicyState>,
    read: &[JobPolicyState],
) -> Vec<JobPolicyState> {
    let on = |kind: PolicyKind| acting.iter().any(|policy| policy.kind == kind);
    let mut job_states: Vec<JobPolicyState> = decided
        .into_iter()
        .cloned()
        .map(|mut job_state| {
            if let Some(table) = table {
                job_state.missing_params.retain(|model| {
                    table
                        .resolve_recorded(
                            model.descriptor.as_deref(),
                            model.vault_descriptor.as_deref(),
                        )
                        .is_none()
                });
            }
            job_state
        })
        .chain(read.iter().cloned())
        .map(|mut job_state| {
            job_state.flagged.remove(&PolicyKind::SmallerFailed);
            if on(PolicyKind::ModelParams) && !job_state.missing_params.is_empty() {
                job_state.flagged.insert(PolicyKind::ModelParams);
            } else {
                job_state.flagged.remove(&PolicyKind::ModelParams);
            }
            job_state
        })
        .collect();
    job_states.sort_by(|a, b| a.job_id.cmp(&b.job_id));
    if on(PolicyKind::SmallerFailed) {
        let below: HashSet<JobId> = failed_below_clean(&job_states);
        job_states
            .iter_mut()
            .filter(|job_state| below.contains(&job_state.job_id))
            .for_each(|job_state| {
                job_state.flagged.insert(PolicyKind::SmallerFailed);
            });
    }
    job_states
}

/// The writes that take the stored job states `before` to `after`.
fn job_state_changes(before: &[JobPolicyState], after: &[JobPolicyState]) -> JobStateChanges {
    let stored: HashMap<&JobId, &JobPolicyState> = before.iter().map(|v| (&v.job_id, v)).collect();
    let kept: HashSet<&JobId> = after.iter().map(|job_state| &job_state.job_id).collect();
    JobStateChanges {
        put: after
            .iter()
            .filter(|job_state| stored.get(&job_state.job_id) != Some(job_state))
            .cloned()
            .collect(),
        removed: before
            .iter()
            .map(|job_state| &job_state.job_id)
            .filter(|job_id| !kept.contains(job_id))
            .cloned()
            .collect(),
    }
}

/// The failed jobs of `job_states` whose family holds a larger job with rows and
/// no failure, as [`smaller_failures`] finds them in rows.
fn failed_below_clean(job_states: &[JobPolicyState]) -> HashSet<JobId> {
    let by_family: HashMap<&str, Vec<(&[i64], &JobPolicyState)>> = job_states
        .iter()
        .filter_map(|job_state| {
            let (key, sizes) = job_state.family.as_ref()?;
            Some((key.as_str(), (sizes.as_slice(), job_state)))
        })
        .fold(HashMap::new(), |mut by_family, (key, job)| {
            by_family.entry(key).or_default().push(job);
            by_family
        });
    by_family
        .values()
        .flat_map(|jobs| {
            jobs.iter()
                .filter(|(sizes, job_state)| {
                    job_state.failed
                        && jobs
                            .iter()
                            .any(|(larger, other)| !other.failed && below(sizes, larger))
                })
                .map(|(_, job_state)| job_state.job_id.clone())
        })
        .collect()
}

/// The kinds of `acting` that flag a job of `job_states`, or whose re-runs in
/// `waiting` still wait for their rows: those the read did not judge.
fn triggered(
    acting: &[CampaignGroupPolicy],
    job_states: &[JobPolicyState],
    waiting: &[PolicyRerun],
    judged: &[PolicyRerun],
) -> BTreeSet<PolicyKind> {
    let judged: BTreeSet<(&JobId, PolicyKind)> = judged
        .iter()
        .map(|rerun| (&rerun.rerun_job_id, rerun.kind))
        .collect();
    let waiting = waiting
        .iter()
        .filter(|rerun| !judged.contains(&(&rerun.rerun_job_id, rerun.kind)))
        .map(|rerun| rerun.kind);
    job_states
        .iter()
        .flat_map(|job_state| job_state.flagged.iter().copied())
        .chain(waiting)
        .filter(|kind| acting.iter().any(|policy| policy.kind == *kind))
        .collect()
}

fn unanswered(campaign_id: &CampaignId, error: ResultsError) {
    tracing::warn!(
        %campaign_id,
        timeout = error.is_timeout(),
        error = %format!("{:#}", anyhow::Error::from(error)),
        "pipette-duckdb did not answer the campaign's rows; it reads unreadable"
    );
}

/// The plan `campaign` runs, if the policies may act on it, else the state
/// that says why they do not.
async fn head(
    plans: &dyn PlanStore,
    group: &CampaignGroup,
    campaign: &Campaign,
) -> anyhow::Result<Result<PlanManifest, PolicyMemberState>> {
    if campaign.owner != group.owner {
        return Ok(Err(PolicyMemberState::OtherOwner));
    }
    let head = match campaign
        .current_plan_id
        .as_ref()
        .filter(|_| campaign.state.is_active())
    {
        Some(plan_id) => plans.get_plan(plan_id).await?,
        None => None,
    };
    let Some(head) = head.filter(|head| head.source_sha256.is_some()) else {
        return Ok(Err(PolicyMemberState::NoPlan));
    };
    if head.status == PlanStatus::Creating {
        return Ok(Err(PolicyMemberState::NotReady));
    }
    Ok(if stopped(plans, &head.plan_id, head.status).await? {
        Err(PolicyMemberState::Cancelled)
    } else {
        Ok(head)
    })
}

/// Whether someone stopped the plan whose status is `status`: it is
/// `cancelling` or `cancelled`, or a cancel marker names it.
async fn stopped(
    plans: &dyn PlanStore,
    plan_id: &PlanId,
    status: PlanStatus,
) -> anyhow::Result<bool> {
    match status {
        PlanStatus::Cancelling | PlanStatus::Cancelled => Ok(true),
        PlanStatus::Creating
        | PlanStatus::Active
        | PlanStatus::PendingClients
        | PlanStatus::Complete => plans.has_cancel_marker(plan_id).await,
    }
}

/// The group's policies that are on among `stored`, in kind order: what a
/// member's record is kept under.
fn acting(group_id: &CampaignGroupId, stored: &[CampaignGroupPolicy]) -> Vec<CampaignGroupPolicy> {
    PolicyKind::iter()
        .map(|kind| kind.policy(group_id, stored))
        .filter(|policy| policy.enabled)
        .collect()
}

/// Whether `a` and `b` hold the same policies that re-run or mark, which a
/// refusal and a filing are kept under: turning model_params on or off
/// changes no filing.
fn same_filers(a: &[CampaignGroupPolicy], b: &[CampaignGroupPolicy]) -> bool {
    fn filers(policies: &[CampaignGroupPolicy]) -> impl Iterator<Item = &CampaignGroupPolicy> {
        policies
            .iter()
            .filter(|policy| policy.kind.reruns_or_marks())
    }
    filers(a).eq(filers(b))
}

/// [`Hold::NotAdvanced`] for the last group filing `state` records, while it
/// still runs and `campaign` does not run it. It is read before the head, so
/// the filing is cancelled even once the head is stopped, archived or handed
/// off.
async fn unadvanced(
    plans: &dyn PlanStore,
    state: &GroupPolicyState,
    campaign: &Campaign,
) -> anyhow::Result<Option<Hold>> {
    let Some(filed) = state
        .filed
        .as_ref()
        .filter(|filed| campaign.current_plan_id.as_ref() != Some(&filed.plan_id))
    else {
        return Ok(None);
    };
    Ok(still_runs(plans, &filed.plan_id)
        .await?
        .then(|| Hold::NotAdvanced {
            plan_id: filed.plan_id.clone(),
            commit_unknown: filed.commit_unknown,
        }))
}

/// What holds `head` before its rows are read: jobs its campaign's last group
/// filing queued that wait for a device, or a refusal of `head` recorded under
/// this build and the policies of `acting` that re-run or mark. The filing is
/// the plan `pass.state.filed` names, and `head` too when that names another
/// plan and `head` is a group filing: the record a lost member row or a failed
/// write did not keep.
async fn held(
    plans: &dyn PlanStore,
    pass: &Pass<'_>,
    head: &PlanManifest,
    acting: &[CampaignGroupPolicy],
) -> anyhow::Result<Option<Hold>> {
    let state = pass.state;
    let filed = state.filed.as_ref();
    let head_filed = filed.is_none_or(|filed| filed.plan_id != head.plan_id)
        && !head.rerun_of.is_empty()
        && group_filing(plans, &head.plan_id).await?;
    let queued: Vec<JobId> = filed
        .into_iter()
        .flat_map(|filed| &filed.queued)
        .chain(head.rerun_of.keys().filter(|_| head_filed))
        .cloned()
        .collect();
    let waiting = pass.todo.avail_for_jobs(&queued).await?;
    if !waiting.is_empty() {
        let plan_id = match filed {
            Some(filed) if !head_filed => filed.plan_id.clone(),
            _ => head.plan_id.clone(),
        };
        return Ok(Some(Hold::Queued {
            plan_id,
            waiting: waiting.len(),
        }));
    }
    Ok(state
        .refusal
        .as_ref()
        .filter(|refusal| {
            refusal.plan_id == head.plan_id
                && same_filers(&state.policies, acting)
                && refusal.build == BUILD_VERSION
        })
        .map(|refusal| Hold::Refused(refusal.reason.clone())))
}

/// Whether `plan_id` still runs: it has not ended, and no one stopped it.
async fn still_runs(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<bool> {
    Ok(match summary(plans, plan_id).await? {
        None => false,
        Some(summary) => {
            summary.status != PlanStatus::Complete
                && !stopped(plans, plan_id, summary.status).await?
        }
    })
}

/// Whether `plan_id` was filed by the policies of a campaign group, any group.
async fn group_filing(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<bool> {
    Ok(summary(plans, plan_id).await?.is_some_and(|summary| {
        matches!(
            summary.change,
            Some(RevisionChange::Rerun {
                group_id: Some(_),
                ..
            })
        )
    }))
}

/// `plan_id`'s summary, with its change; `None` if it is gone.
async fn summary(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<Option<PlanSummary>> {
    let read = plans
        .get_plan_summaries(std::slice::from_ref(plan_id))
        .await?;
    if let Some(unreadable) = read.unreadable.into_iter().next() {
        return Err(unreadable.into());
    }
    Ok(read.items.into_iter().next())
}

/// Check `answered`, jobs of `head`: one row read covers them, a job without
/// rows waits, and each flagged job's chain is walked before its [`Outcome`]
/// counts. With `table`, the rows read are also checked for model params. Each
/// of the `waiting` re-runs is judged on its rows once they land; the rows of
/// one `head` no longer holds are read apart, and such a one without rows is
/// dropped if `unrecorded` holds its job.
#[expect(
    clippy::too_many_arguments,
    reason = "each is distinct context the caller already holds; a bundle type \
              would be single-use"
)]
async fn judge(
    plans: &dyn PlanStore,
    results: &ResultsDb,
    group_id: &CampaignGroupId,
    campaign_id: &CampaignId,
    head: &PlanManifest,
    policies: &[CampaignGroupPolicy],
    table: Option<&ModelParamTable>,
    families: Option<&Families>,
    answered: &[JobId],
    waiting: &[PolicyRerun],
    unrecorded: &HashSet<JobId>,
    bodies: Option<(&dyn SubmissionStore, BodyReads<'_>)>,
    mut evaluation: Evaluation,
) -> Result<Evaluation, ResultsError> {
    let rows = rows_by_job(results.all_job_results(answered).await?);
    if let Some(table) = table {
        evaluation = with_params(table, &rows, evaluation);
    }
    evaluation = with_smaller_failed(families, &rows, evaluation);
    let without_rows: Vec<JobId> = answered
        .iter()
        .filter(|job_id| !rows.contains_key(*job_id))
        .cloned()
        .collect();
    let read_bodies = match bodies {
        Some((submissions, bodies)) => {
            evaluation.body_reads = without_rows.len();
            body_rows(submissions, bodies.catalog, without_rows).await
        }
        None => HashMap::new(),
    };
    let mut chains: BTreeMap<&JobId, Chain<'_>> = answered
        .iter()
        .filter_map(|job_id| {
            let rows = rows
                .get(job_id)
                .or_else(|| read_bodies.get(job_id).map(|(_, rows)| rows))?;
            let flagging: Vec<&CampaignGroupPolicy> = policies
                .iter()
                .filter(|policy| flags(policy, rows))
                .collect();
            let skip =
                (!head.content_keys.contains_key(job_id)).then_some(PolicySkipReason::NoContentKey);
            (!flagging.is_empty()).then(|| (job_id, Chain::new(job_id, rows, flagging, skip)))
        })
        .collect();
    walk(plans, results, campaign_id, head, &mut chains).await?;
    // A body's rows can change once the scorer scores it, so they act only by a
    // re-run, whose wrong flag costs one more run; any other outcome waits for
    // the rows in pipette-duckdb.
    let rerunning: Vec<(JobId, [u8; 32])> = read_bodies
        .into_iter()
        .filter(|(job_id, _)| {
            chains
                .get(job_id)
                .and_then(|chain| chain.outcome(|policy| policy.enabled))
                .is_some_and(|outcome| matches!(outcome, Outcome::Rerun))
        })
        .map(|(job_id, (fingerprint, _))| (job_id, fingerprint))
        .collect();
    let named = match bodies {
        Some((submissions, bodies)) => {
            evaluation.body_reads += rerunning.len();
            named_rows(submissions, bodies, rerunning).await
        }
        None => HashMap::new(),
    };
    evaluation.from_body = named.keys().cloned().collect();
    let rows_of = |job_id: &JobId| rows.get(job_id).or_else(|| named.get(job_id));
    let (scored, unscored): (Vec<&JobId>, Vec<&JobId>) = answered
        .iter()
        .partition(|job_id| rows_of(job_id).is_some());
    evaluation.waiting_job_count = unscored.len();
    scored
        .iter()
        .for_each(|job_id| evaluation.count(job_id, chains.get(job_id), group_id));
    let missing = table
        .map(|table| missing_by_job(table, &rows))
        .unwrap_or_default();
    evaluation.read = scored
        .iter()
        .map(|&job_id| JobPolicyState {
            job_id: job_id.clone(),
            flagged: evaluation.flagged.get(job_id).cloned().unwrap_or_default(),
            decided: evaluation.decided.contains(job_id),
            missing_params: missing.get(job_id).cloned().unwrap_or_default(),
            family: families.and_then(|families| families.get(job_id).cloned()),
            failed: rows_of(job_id).is_some_and(|rows| failed_row(rows).is_some()),
        })
        .collect();
    evaluation.reruns = evaluation
        .rerun_job_ids
        .iter()
        .filter_map(|job_id| {
            let (chain, rows) = (chains.get(job_id)?, rows_of(job_id)?);
            let reruns = chain
                .rerunning()
                .filter_map(|tally| Rerunning::new(tally.policy, tally.attempts, rows))
                .collect();
            Some((job_id.clone(), reruns))
        })
        .collect();
    let held: HashSet<&JobId> = head.job_ids.iter().collect();
    let left: Vec<JobId> = waiting
        .iter()
        .map(|rerun| rerun.rerun_job_id.clone())
        .filter(|job_id| !held.contains(job_id))
        .collect();
    let left_rows = rows_by_job(results.all_job_results(&left).await?);
    let at = Timestamp::now();
    evaluation.judged = waiting
        .iter()
        .filter_map(|rerun| {
            let job_id = &rerun.rerun_job_id;
            match rows.get(job_id).or_else(|| left_rows.get(job_id)) {
                Some(rows) => Some(judged(group_id, rerun, rows, at)),
                None => unrecorded.contains(job_id).then(|| dropped(rerun, at)),
            }
        })
        .collect();
    evaluation
        .skipped
        .iter()
        .filter(|(_, reason)| for_good(**reason))
        .for_each(|(job_id, reason)| {
            tracing::warn!(
                %campaign_id,
                %job_id,
                ?reason,
                "the group policies leave a flagged job as it is"
            );
        });
    Ok(evaluation)
}

/// The jobs of the `waiting` re-runs that `head` no longer holds and that have
/// no recorded result. One with a record waits: its rows reach pipette-duckdb
/// after it.
async fn unrecorded(
    submissions: &dyn SubmissionStore,
    head: &PlanManifest,
    waiting: &[PolicyRerun],
) -> anyhow::Result<HashSet<JobId>> {
    let held: HashSet<&JobId> = head.job_ids.iter().collect();
    let left: HashSet<JobId> = waiting
        .iter()
        .map(|rerun| &rerun.rerun_job_id)
        .filter(|job_id| !held.contains(job_id))
        .cloned()
        .collect();
    // Owned ids: a stream of borrowed ones trips higher-ranked `Send`
    // inference in the handlers that await this.
    futures::stream::iter(left)
        .map(|job_id| async move {
            let recorded = submissions.has_record(&job_id).await?;
            anyhow::Ok((!recorded).then_some(job_id))
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_filter_map(|job_id| async move { Ok(job_id) })
        .try_collect()
        .await
}

/// The rows that [`policy_rows`] builds from the submission body of each of
/// `jobs`, with a fingerprint instead of the full body. Missing, unreadable
/// or invalid bodies leave the job waiting for its rows in pipette-duckdb.
/// A held submission has no body here: `find_job` does not read the unverified
/// archive.
async fn body_rows(
    submissions: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    jobs: Vec<JobId>,
) -> HashMap<JobId, ([u8; 32], Vec<ResultRow>)> {
    futures::stream::iter(jobs)
        .map(|job_id| async move {
            let read = async {
                let Some(record) = submissions.find_job(&job_id).await? else {
                    return Ok(None);
                };
                let submission = parse_stored_submission(&record.body)?;
                let rows = policy_rows(catalog, &submission, &VaultNames::default())?;
                rows.map(|rows| anyhow::Ok((body_fingerprint(&record.body)?, rows)))
                    .transpose()
            };
            match read.await {
                Ok(read) => read.map(|read| (job_id, read)),
                Err(error) => {
                    tracing::debug!(
                        %job_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: a submission body gave no policy rows; the job waits \
                         for its rows"
                    );
                    None
                }
            }
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .filter_map(std::future::ready)
        .collect()
        .await
}

/// Hash the JSON into a fixed-size fingerprint without allocating its text.
fn body_fingerprint(body: &Value) -> anyhow::Result<[u8; 32]> {
    struct HashWriter(Sha256);

    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut hash = HashWriter(Sha256::new());
    serde_json::to_writer(&mut hash, body)?;
    Ok(hash.0.finalize().into())
}

/// Read each re-run candidate again and build rows with the scorer's vault
/// names. Changed or missing bodies wait for the next pass. Failed body or
/// name reads also leave the job waiting.
async fn named_rows(
    submissions: &dyn SubmissionStore,
    bodies: BodyReads<'_>,
    rerunning: Vec<(JobId, [u8; 32])>,
) -> HashMap<JobId, Vec<ResultRow>> {
    futures::stream::iter(rerunning)
        .map(|(job_id, fingerprint)| async move {
            let read = async {
                let Some(record) = submissions.find_job(&job_id).await? else {
                    return Ok(None);
                };
                if body_fingerprint(&record.body)? != fingerprint {
                    tracing::debug!(%job_id, "queue-maintenance: a re-run candidate's submission body changed; the job waits for the next pass");
                    return Ok(None);
                }
                let submission = parse_stored_submission(&record.body)?;
                let names = VaultNames::of(bodies.stores, &submission).await?;
                policy_rows(bodies.catalog, &submission, &names)
            };
            match read.await {
                Ok(rows) => rows.map(|rows| (job_id, rows)),
                Err(error) => {
                    tracing::warn!(
                        %job_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: a flagged submission body or its vault names did not \
                         read; the job waits for its rows"
                    );
                    None
                }
            }
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .filter_map(std::future::ready)
        .collect()
        .await
}

/// `evaluation` with the gaps of `rows` in `table`, whose jobs the model
/// params check flags and, as it only flags, keeps.
fn with_params(
    table: &ModelParamTable,
    rows: &HashMap<JobId, Vec<ResultRow>>,
    mut evaluation: Evaluation,
) -> Evaluation {
    let (missing, jobs) = missing_params(table, rows);
    evaluation.missing_model_params = missing;
    evaluation
        .checks
        .iter_mut()
        .filter(|check| check.kind == PolicyKind::ModelParams)
        .for_each(|check| {
            check.flagged = jobs;
            check.kept = jobs;
        });
    evaluation
}

/// Each job's family key and sizes, as [`size_family`] gives them.
type Families = HashMap<JobId, (String, Vec<i64>)>;

/// `Sources` reads and caches the job sources once so that [`performance_jobs`]
/// and [`families`] share the result. A source that does not decode fails the read.
struct Sources<'a> {
    plans: &'a dyn PlanStore,
    jobs: Vec<JobId>,
    read: OnceCell<HashMap<JobId, Value>>,
}

impl<'a> Sources<'a> {
    fn new(plans: &'a dyn PlanStore, jobs: Vec<JobId>) -> Self {
        Self {
            plans,
            jobs,
            read: OnceCell::new(),
        }
    }

    /// The sources of `wanted`, a subset of `jobs`. The first use reads only
    /// `wanted`; a later use takes from that read, so it names a subset of the
    /// first use's jobs.
    async fn bodies(&self, wanted: &[JobId]) -> anyhow::Result<&HashMap<JobId, Value>> {
        self.read
            .get_or_try_init(|| async {
                let sources = self.plans.get_job_sources(wanted).await?;
                if let Some(bad) = sources.unreadable.first() {
                    anyhow::bail!("job source {} does not decode: {}", bad.key, bad.reason);
                }
                Ok(sources.items.into_iter().collect())
            })
            .await
    }
}

/// Find the families of jobs whose benchmark types have sizes. Use `sources`
/// and `catalog` for `campaign_id`. Return `None` without a catalog. A job with
/// no source has no family.
async fn families(
    sources: &Sources<'_>,
    catalog: Option<&HashMap<BenchmarkId, Benchmark>>,
    campaign_id: &CampaignId,
    jobs: &[JobId],
) -> anyhow::Result<Option<Families>> {
    let Some(catalog) = catalog else {
        return Ok(None);
    };
    let bodies = sources.bodies(jobs).await?;
    jobs.iter()
        .filter_map(|job_id| {
            let body = bodies.get(job_id)?;
            size_family(body, catalog, campaign_id)
                .with_context(|| format!("finding the family of job {job_id}"))
                .map(|family| family.map(|family| (job_id.clone(), family)))
                .transpose()
        })
        .collect::<anyhow::Result<_>>()
        .map(Some)
}

/// The jobs of `sources`, all `plan`'s, that the policies judge: those of a
/// performance benchmark. An eval's cell (`eval`, `vlm_eval`, `asr_eval`), which
/// the server scores, is never flagged, re-run or marked. A job is typed by the
/// benchmark its source names, as `plan` types its cells; a plan that recorded
/// its types, none an eval, reads no source.
async fn performance_jobs(
    plan: &PlanManifest,
    sources: &Sources<'_>,
) -> anyhow::Result<Vec<JobId>> {
    let recorded = &plan.benchmark_types;
    if !recorded.is_empty() && !recorded.values().any(|kind| kind.is_scored_via_server()) {
        return Ok(sources.jobs.clone());
    }
    let benchmarks: HashMap<&JobId, BenchmarkId> = sources
        .bodies(&sources.jobs)
        .await?
        .iter()
        .filter_map(|(job_id, body)| {
            let id = body.pointer("/spec/benchmark")?.as_str()?;
            Some((job_id, BenchmarkId::try_new(id).ok()?))
        })
        .collect();
    let types = crate::plan_cells::filed_types(recorded, benchmarks.values());
    Ok(sources
        .jobs
        .iter()
        .filter(|job_id| {
            !benchmarks
                .get(job_id)
                .and_then(|id| types.get(id))
                .is_some_and(|kind| kind.is_scored_via_server())
        })
        .cloned()
        .collect())
}

/// `evaluation` with the failed jobs of `rows` that a larger job of their
/// family answered clean, which the smaller_failed check flags and, as it
/// only flags, keeps; as it is without `families`.
fn with_smaller_failed(
    families: Option<&Families>,
    rows: &HashMap<JobId, Vec<ResultRow>>,
    mut evaluation: Evaluation,
) -> Evaluation {
    let Some(families) = families else {
        return evaluation;
    };
    let found = smaller_failures(families, rows);
    evaluation
        .checks
        .iter_mut()
        .filter(|check| check.kind == PolicyKind::SmallerFailed)
        .for_each(|check| {
            check.flagged = found.len();
            check.kept = found.len();
        });
    evaluation.smaller_failed = found;
    evaluation
}

/// A job of a family: its id, its sizes and its rows.
type SizedJob<'r> = (&'r JobId, &'r [i64], &'r [ResultRow]);

/// The failed jobs of `rows` whose family in `families` holds a larger job with
/// rows and no failure, each with the smallest such job, in benchmark id then
/// job id order.
fn smaller_failures(
    families: &Families,
    rows: &HashMap<JobId, Vec<ResultRow>>,
) -> Vec<SmallerFailure> {
    let by_family: HashMap<&str, Vec<SizedJob<'_>>> = rows
        .iter()
        .filter_map(|(job_id, rows)| {
            let (key, sizes) = families.get(job_id)?;
            Some((key.as_str(), (job_id, sizes.as_slice(), rows.as_slice())))
        })
        .fold(HashMap::new(), |mut by_family, (key, job)| {
            by_family.entry(key).or_default().push(job);
            by_family
        });
    let mut found: Vec<SmallerFailure> = by_family
        .values()
        .flat_map(|jobs| {
            jobs.iter().filter_map(|&(job_id, sizes, rows)| {
                let failed = failed_row(rows)?;
                let &(clean_job_id, _, clean) = jobs
                    .iter()
                    .filter(|&&(_, larger, rows)| {
                        below(sizes, larger) && failed_row(rows).is_none()
                    })
                    .min_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)))?;
                Some(SmallerFailure {
                    job_id: job_id.clone(),
                    benchmark_id: failed.benchmark_id.clone(),
                    failure_source: failed.failure_source,
                    failure_code: failed.failure_code,
                    model_key: clean.iter().find_map(model_key),
                    clean_job_id: clean_job_id.clone(),
                    clean_benchmark_id: clean.first()?.benchmark_id.clone(),
                })
            })
        })
        .collect();
    found.sort_by(|a, b| (&a.benchmark_id, &a.job_id).cmp(&(&b.benchmark_id, &b.job_id)));
    found
}

/// The first failed row of a run's `rows`.
fn failed_row(rows: &[ResultRow]) -> Option<&ResultRow> {
    rows.iter().find(|row| row.failed == Some(true))
}

/// Whether a run of `smaller` sizes is below one of `larger` sizes of its
/// family, as the queue's `queue_sizes_below` reads them: no size larger, and
/// not every size the same.
fn below(smaller: &[i64], larger: &[i64]) -> bool {
    smaller != larger && smaller.iter().zip(larger).all(|(s, l)| s <= l)
}

impl Evaluation {
    /// Count an answered job with rows, walked as `chain` when a check flags it.
    fn count(&mut self, job_id: &JobId, chain: Option<&Chain<'_>>, group_id: &CampaignGroupId) {
        let flagging: BTreeSet<PolicyKind> = chain
            .into_iter()
            .flat_map(|chain| &chain.tallies)
            .filter(|tally| tally.policy.enabled)
            .map(|tally| tally.policy.kind)
            .collect();
        if !flagging.is_empty() {
            self.flagged.insert(job_id.clone(), flagging);
        }
        self.checks.iter_mut().for_each(|check| {
            let kind = check.kind;
            let Some(outcome) = chain.and_then(|chain| chain.outcome(|policy| policy.kind == kind))
            else {
                return;
            };
            check.flagged += 1;
            match outcome {
                Outcome::Rerun => check.rerun += 1,
                Outcome::Mark { .. } => check.marked += 1,
                Outcome::Keep => check.kept += 1,
                Outcome::Skip(_) => {}
            }
        });
        match chain.and_then(|chain| chain.outcome(|policy| policy.enabled)) {
            None | Some(Outcome::Keep) => {
                self.kept_job_count += 1;
                self.decided.insert(job_id.clone());
            }
            Some(Outcome::Rerun) => {
                self.rerun_job_ids.insert(job_id.clone());
                self.kinds.extend(
                    chain
                        .into_iter()
                        .flat_map(Chain::rerunning)
                        .map(|tally| tally.policy.kind),
                );
            }
            Some(Outcome::Mark { kind, spent }) => {
                self.marked
                    .insert(job_id.clone(), mark_reason(group_id, kind, spent));
                self.kinds.insert(kind);
            }
            Some(Outcome::Skip(reason)) => {
                if for_good(reason) {
                    self.decided.insert(job_id.clone());
                }
                self.skipped.insert(job_id.clone(), reason);
            }
        }
    }
}

/// Whether a job skipped for `reason` is left as it is for good, rather than
/// read again next time.
fn for_good(reason: PolicySkipReason) -> bool {
    match reason {
        PolicySkipReason::NoContentKey | PolicySkipReason::UnreadablePlan => true,
        PolicySkipReason::ChainReadFailed | PolicySkipReason::EarlierRunWithoutRows => false,
    }
}

/// What the policies that flag a job make of it: re-run it while one of them
/// has a re-run left and its chain has fewer than [`CHAIN_RUNS`] runs, else
/// mark it failed if one that marks is among them, else keep it.
enum Outcome {
    Rerun,
    /// The first of them that marks ([`PolicyKind::marks`]), with its flagged
    /// runs in a row and its re-runs when it spent them; `None` when the chain
    /// is full instead.
    Mark {
        kind: PolicyKind,
        spent: Option<(u32, u32)>,
    },
    Keep,
    /// Taken before the rule: the job has no content key, or the walk stopped
    /// short of what one of them needs.
    Skip(PolicySkipReason),
}

/// The reason a mark's failure record carries, naming the `kind` policy that
/// marked it.
fn mark_reason(group_id: &CampaignGroupId, kind: PolicyKind, spent: Option<(u32, u32)>) -> String {
    let why = match spent {
        Some((attempts, max_reruns)) => format!(
            "flagged on {attempts} run{} in a row, {max_reruns} re-run{} spent",
            if attempts == 1 { "" } else { "s" },
            if max_reruns == 1 { "" } else { "s" },
        ),
        None => format!("its re-run chain has {CHAIN_RUNS} runs"),
    };
    format!("The {kind} policy of campaign group {group_id} marked this job failed: {why}.")
}

/// One flagged job's walk back along its re-run chain.
struct Chain<'p> {
    /// The policies whose checks flag the job, each with its count.
    tallies: Vec<Tally<'p>>,
    /// The earliest run reached: the job, then the run it ran again, and so on.
    at: JobId,
    /// The runs reached, the job's own included.
    runs: u32,
    /// `at` is the chain's first job.
    first: bool,
    /// `at` was reached while a check counts, so its rows are read next.
    unscored: bool,
    /// Set before the walk; it stops every check.
    skip: Option<PolicySkipReason>,
}

/// A check's attempts on one job: its runs in a row that the check flags,
/// newest first.
struct Tally<'p> {
    policy: &'p CampaignGroupPolicy,
    /// The most re-runs the policy gives the job, as [`rerun_limit`] reads it.
    limit: u32,
    attempts: u32,
    /// Every run so far is flagged and the re-runs are not spent, so the next
    /// run counts too.
    counting: bool,
    /// Why the walk stopped short of what this check needs.
    skip: Option<PolicySkipReason>,
}

impl Tally<'_> {
    fn has_budget(&self) -> bool {
        self.attempts <= self.limit
    }

    /// Whether the check still needs the walk: it would re-run the job unless
    /// the chain is full.
    fn open(&self) -> bool {
        self.skip.is_none() && self.has_budget()
    }

    fn stop(&mut self, reason: PolicySkipReason) {
        self.skip = Some(reason);
        self.counting = false;
    }
}

impl<'p> Chain<'p> {
    fn new(
        job_id: &JobId,
        rows: &[ResultRow],
        flagging: Vec<&'p CampaignGroupPolicy>,
        skip: Option<PolicySkipReason>,
    ) -> Self {
        Self {
            tallies: flagging
                .into_iter()
                .map(|policy| {
                    let limit = rerun_limit(policy, rows);
                    Tally {
                        policy,
                        limit,
                        attempts: 1,
                        counting: limit >= 1,
                        skip: None,
                    }
                })
                .collect(),
            at: job_id.clone(),
            runs: 1,
            first: false,
            unscored: false,
            skip,
        }
    }

    /// Whether the walk reads the run before `at`: a check would re-run the
    /// job, and its chain may not yet hold [`CHAIN_RUNS`] runs.
    fn goes_on(&self) -> bool {
        self.skip.is_none()
            && !self.first
            && self.runs < CHAIN_RUNS
            && self.tallies.iter().any(Tally::open)
    }

    /// Step back to the run `link` names. A link that did not read stops the
    /// checks that still need the walk.
    fn follow(&mut self, link: Option<&Result<JobId, PolicySkipReason>>) {
        match link {
            None => self.first = true,
            Some(Ok(run)) => {
                self.at = run.clone();
                self.runs += 1;
                self.unscored = self.tallies.iter().any(|tally| tally.counting);
            }
            Some(Err(reason)) => self
                .tallies
                .iter_mut()
                .filter(|tally| tally.open())
                .for_each(|tally| tally.stop(*reason)),
        }
    }

    /// Count the run at `at`, whose rows are `rows`. A mark, which no device
    /// ran, is flagged by no check, so it ends every count; a run without rows
    /// stops the checks still counting.
    fn score(&mut self, rows: Option<&[ResultRow]>) {
        self.unscored = false;
        self.tallies
            .iter_mut()
            .filter(|tally| tally.counting)
            .for_each(|tally| match rows {
                None => tally.stop(PolicySkipReason::EarlierRunWithoutRows),
                Some(rows) if flags(tally.policy, rows) => {
                    tally.attempts += 1;
                    tally.counting = tally.has_budget();
                }
                Some(_) => tally.counting = false,
            });
    }

    /// The [`Outcome`] of the policies `weighed` takes; `None` when none of
    /// them flags the job.
    fn outcome(&self, weighed: impl Fn(&CampaignGroupPolicy) -> bool) -> Option<Outcome> {
        let tallies: Vec<&Tally<'_>> = self
            .tallies
            .iter()
            .filter(|tally| weighed(tally.policy))
            .collect();
        if tallies.is_empty() {
            return None;
        }
        let marking = tallies.iter().find(|tally| tally.policy.kind.marks());
        let skip = self
            .skip
            .or_else(|| tallies.iter().find_map(|tally| tally.skip));
        Some(if let Some(reason) = skip {
            Outcome::Skip(reason)
        } else if self.runs < CHAIN_RUNS && tallies.iter().any(|tally| tally.has_budget()) {
            Outcome::Rerun
        } else if let Some(marking) = marking {
            Outcome::Mark {
                kind: marking.policy.kind,
                spent: (!marking.has_budget()).then_some((marking.attempts, marking.limit)),
            }
        } else {
            Outcome::Keep
        })
    }

    /// The tallies of the policies that are on and flag the job with a re-run
    /// left: the ones that re-run it.
    fn rerunning(&self) -> impl Iterator<Item = &Tally<'p>> {
        self.tallies
            .iter()
            .filter(|tally| tally.policy.enabled && tally.has_budget())
    }
}

/// Walk each chain back from its plan's job, one earlier run a step: the
/// first link is the plan's own `rerun_of`, each later one a `job_holders`
/// read of every run reached, and each step reads the rows of the runs it
/// reached while a check still counts on them.
async fn walk(
    plans: &dyn PlanStore,
    results: &ResultsDb,
    campaign_id: &CampaignId,
    head: &PlanManifest,
    chains: &mut BTreeMap<&JobId, Chain<'_>>,
) -> Result<(), ResultsError> {
    futures::stream::iter((1..CHAIN_RUNS).map(Ok::<u32, ResultsError>))
        .try_fold(chains, |chains, step| async move {
            let reached: BTreeSet<JobId> = chains
                .values()
                .filter(|chain| chain.goes_on())
                .map(|chain| chain.at.clone())
                .collect();
            if reached.is_empty() {
                return Ok(chains);
            }
            let links: BTreeMap<JobId, Result<JobId, PolicySkipReason>> = if step == 1 {
                reached
                    .iter()
                    .filter_map(|job_id| {
                        Some((job_id.clone(), Ok(head.rerun_of.get(job_id)?.clone())))
                    })
                    .collect()
            } else {
                holder_links(plans, campaign_id, &reached).await
            };
            chains
                .values_mut()
                .filter(|chain| chain.goes_on())
                .for_each(|chain| {
                    let link = links.get(&chain.at);
                    chain.follow(link);
                });
            let unscored: Vec<JobId> = chains
                .values()
                .filter(|chain| chain.unscored)
                .map(|chain| chain.at.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            if !unscored.is_empty() {
                let rows = rows_by_job(results.all_job_results(&unscored).await?);
                chains
                    .values_mut()
                    .filter(|chain| chain.unscored)
                    .for_each(|chain| {
                        let run = rows.get(&chain.at).map(Vec::as_slice);
                        chain.score(run);
                    });
            }
            Ok(chains)
        })
        .await?;
    Ok(())
}

/// The run before each of `reached`, from its newest holder plan in the
/// campaign that decodes, or why it did not read; none for a chain's first
/// job.
async fn holder_links(
    plans: &dyn PlanStore,
    campaign_id: &CampaignId,
    reached: &BTreeSet<JobId>,
) -> BTreeMap<JobId, Result<JobId, PolicySkipReason>> {
    let jobs: Vec<JobId> = reached.iter().cloned().collect();
    let read = match plans.job_holders(campaign_id, &jobs).await {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(
                %campaign_id,
                error = %format!("{error:#}"),
                "the plans holding the jobs' earlier runs did not read; the jobs wait"
            );
            return reached
                .iter()
                .map(|job_id| (job_id.clone(), Err(PolicySkipReason::ChainReadFailed)))
                .collect();
        }
    };
    let read = &read;
    futures::stream::iter(reached)
        .filter_map(|job_id| async move {
            let link = match newest(read, job_id) {
                Some(holder) => holder.rerun_of.clone().map(Ok),
                None if read.unreadable.is_empty() => None,
                // The read names a plan that does not decode by its key alone,
                // and it may hold this job.
                None => alone(plans, campaign_id, job_id).await,
            };
            Some((job_id.clone(), link?))
        })
        .collect()
        .await
}

/// The run before `job_id`, from a read of its holders alone.
async fn alone(
    plans: &dyn PlanStore,
    campaign_id: &CampaignId,
    job_id: &JobId,
) -> Option<Result<JobId, PolicySkipReason>> {
    let read = match plans
        .job_holders(campaign_id, std::slice::from_ref(job_id))
        .await
    {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(
                %campaign_id,
                %job_id,
                error = %format!("{error:#}"),
                "the plans holding a job's earlier run did not read; the job waits"
            );
            return Some(Err(PolicySkipReason::ChainReadFailed));
        }
    };
    match newest(&read, job_id) {
        Some(holder) => holder.rerun_of.clone().map(Ok),
        None if read.unreadable.is_empty() => None,
        None => {
            tracing::warn!(
                %campaign_id,
                earlier_run = %job_id,
                unreadable = ?read.unreadable,
                "a re-run chain runs through a plan that does not decode"
            );
            Some(Err(PolicySkipReason::UnreadablePlan))
        }
    }
}

/// The newest of `job_id`'s holders that names its earlier run, else the
/// newest. A job's earlier run is fixed when it is minted, so every holder
/// that names one names the same, but a revision that takes the job from an
/// older plan names none.
fn newest<'r>(read: &'r JobHolderListing, job_id: &JobId) -> Option<&'r JobHolder> {
    read.items
        .iter()
        .filter(|holder| holder.job_id == *job_id)
        .max_by_key(|holder| {
            (
                holder.rerun_of.is_some(),
                holder.created_at,
                &holder.plan_id,
            )
        })
}

/// Whether `policy`'s check flags a run with `rows`, as the console reads them.
fn flags(policy: &CampaignGroupPolicy, rows: &[ResultRow]) -> bool {
    match policy.kind {
        PolicyKind::Swap => rows
            .iter()
            .filter_map(max_swap_bytes)
            .any(|bytes| bytes > 0.0),
        PolicyKind::Spread => rows.iter().any(|row| {
            spread(row)
                .zip(limit(&policy.settings, row))
                .is_some_and(|(spread, limit)| spread > limit)
        }),
        PolicyKind::Failed => rows.iter().any(|row| flagged_failure(policy, row)),
        // The console's `overBy`: the excess rounded to a tenth of a percent,
        // so one that prints as 0.0 % is not over.
        PolicyKind::TtftOverE2e => CLOCKS
            .iter()
            .filter_map(|pairs| share(rows, pairs))
            .any(|share| ((share - 100.0) * 10.0).round() > 0.0),
        // They re-run and mark nothing: `missing_params` and
        // `smaller_failures` read their rows.
        PolicyKind::ModelParams | PolicyKind::SmallerFailed => false,
    }
}

/// The quantity `policy`'s check weighs against its [`threshold`] on a run's
/// `rows`, which [`flags`] flags over it: the largest swap in bytes, the
/// largest spread or TTFT share of E2E as a percent, and for failed 1 when a
/// row is a device failure with one of its codes, else 0. `None` when no row
/// has it.
fn value(policy: &CampaignGroupPolicy, rows: &[ResultRow]) -> Option<f64> {
    match policy.kind {
        PolicyKind::Swap => rows
            .iter()
            .filter_map(max_swap_bytes)
            .max_by(f64::total_cmp),
        PolicyKind::Spread => rows.iter().filter_map(spread).max_by(f64::total_cmp),
        PolicyKind::Failed => Some(if rows.iter().any(|row| flagged_failure(policy, row)) {
            1.0
        } else {
            0.0
        }),
        PolicyKind::TtftOverE2e => CLOCKS
            .iter()
            .filter_map(|pairs| share(rows, pairs))
            .max_by(f64::total_cmp),
        PolicyKind::ModelParams | PolicyKind::SmallerFailed => None,
    }
}

/// What `policy`'s [`value`] is weighed against.
fn threshold(policy: &CampaignGroupPolicy) -> Option<f64> {
    match policy.kind {
        PolicyKind::Swap => Some(0.0),
        PolicyKind::Spread => policy.settings.percent,
        PolicyKind::TtftOverE2e => Some(100.0),
        PolicyKind::Failed | PolicyKind::ModelParams | PolicyKind::SmallerFailed => None,
    }
}

fn max_swap_bytes(row: &ResultRow) -> Option<f64> {
    row.observations
        .get("max_swap_bytes")
        .and_then(Value::as_f64)
}

/// Whether `row` is a failure its device reported with one of `policy`'s codes.
fn flagged_failure(policy: &CampaignGroupPolicy, row: &ResultRow) -> bool {
    row.failed == Some(true)
        && row.failure_source == Some(FailureSource::Client)
        && row
            .failure_code
            .zip(policy.settings.codes.as_ref())
            .is_some_and(|(code, codes)| codes.contains_key(&code))
}

/// The most re-runs `policy` gives a run with `rows`: for failed, those of the
/// code of the run's first failure it flags, else its `max_reruns`.
fn rerun_limit(policy: &CampaignGroupPolicy, rows: &[ResultRow]) -> u32 {
    match &policy.settings.codes {
        Some(codes) => rows
            .iter()
            .find(|row| flagged_failure(policy, row))
            .and_then(|row| codes.get(&row.failure_code?).copied())
            .unwrap_or(0),
        None => policy.max_reruns,
    }
}

/// For failed, the failure code of a run's rows: a failure with one of the
/// policy's codes first, else any failure's.
fn failure_code(policy: &CampaignGroupPolicy, rows: &[ResultRow]) -> Option<String> {
    if policy.kind != PolicyKind::Failed {
        return None;
    }
    let failed = rows
        .iter()
        .find(|row| flagged_failure(policy, row))
        .or_else(|| rows.iter().find(|row| row.failed == Some(true)))?;
    failed.failure_code.map(|code| code.as_ref().to_owned())
}

/// The key a row's model takes in the model params table: its model
/// descriptor's, else its vault descriptor's.
fn model_key(row: &ResultRow) -> Option<String> {
    [
        row.model_descriptor.as_deref(),
        row.model_vault_descriptor.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(descriptor_key)
}

/// One policy's re-run of a job, as a filing records it, but for the new job,
/// its plan and its time.
#[derive(Debug, Clone, PartialEq)]
pub struct Rerunning {
    pub kind: PolicyKind,
    /// The runs in a row the policy flagged, the job's own included.
    pub attempt: u32,
    pub benchmark_id: String,
    pub model_key: Option<String>,
    pub before: f64,
    pub threshold: Option<f64>,
    pub short_repetition_ms: Option<f64>,
    pub short_repetition_percent: Option<f64>,
    pub codes: Option<Vec<FailureCode>>,
    pub before_code: Option<String>,
}

impl Rerunning {
    /// `policy`'s re-run of a job whose `rows` it flags, on `attempt` runs in
    /// a row; `None` without a row, or a value of its check.
    fn new(policy: &CampaignGroupPolicy, attempt: u32, rows: &[ResultRow]) -> Option<Self> {
        Some(Self {
            kind: policy.kind,
            attempt,
            benchmark_id: rows.first()?.benchmark_id.clone(),
            model_key: rows.iter().find_map(model_key),
            before: value(policy, rows)?,
            threshold: threshold(policy),
            short_repetition_ms: policy.settings.short_repetition_ms,
            short_repetition_percent: policy.settings.short_repetition_percent,
            codes: policy
                .settings
                .codes
                .as_ref()
                .map(|codes| codes.keys().copied().collect()),
            before_code: failure_code(policy, rows),
        })
    }

    /// The row of the re-run of `of` as `job`, which the campaign's filed
    /// `plan` holds, waiting for its rows.
    fn waiting(
        &self,
        campaign_id: &CampaignId,
        plan: &PlanManifest,
        job: &JobId,
        of: &JobId,
    ) -> PolicyRerun {
        PolicyRerun {
            campaign_id: campaign_id.clone(),
            kind: self.kind,
            rerun_job_id: job.clone(),
            rerun_of: of.clone(),
            plan_id: plan.plan_id.clone(),
            filed_at: plan.created_at,
            attempt: self.attempt,
            benchmark_id: self.benchmark_id.clone(),
            model_key: self.model_key.clone(),
            before: self.before,
            threshold: self.threshold,
            short_repetition_ms: self.short_repetition_ms,
            short_repetition_percent: self.short_repetition_percent,
            codes: self.codes.clone(),
            before_code: self.before_code.clone(),
            after: None,
            after_code: None,
            outcome: None,
            judged_at: None,
        }
    }
}

/// `waiting` once its re-run's `rows` are read at `at`: its check's value on
/// them, and whether the check flags them at the settings it was filed at,
/// else whether a row of them failed.
fn judged(
    group_id: &CampaignGroupId,
    waiting: &PolicyRerun,
    rows: &[ResultRow],
    at: Timestamp,
) -> PolicyRerun {
    let filed = CampaignGroupPolicy {
        // `flags` reads the settings of spread and failed alone, and failed's
        // codes but not their limits.
        settings: PolicySettings {
            percent: waiting.threshold,
            short_repetition_ms: waiting.short_repetition_ms,
            short_repetition_percent: waiting.short_repetition_percent,
            codes: waiting
                .codes
                .as_ref()
                .map(|codes| codes.iter().map(|code| (*code, 0)).collect()),
        },
        ..waiting.kind.defaults(group_id.clone())
    };
    let outcome = if flags(&filed, rows) {
        RerunOutcome::Flagged
    } else if rows.iter().any(|row| row.failed == Some(true)) {
        RerunOutcome::Failed
    } else {
        RerunOutcome::Cleared
    };
    PolicyRerun {
        after: value(&filed, rows),
        after_code: failure_code(&filed, rows),
        outcome: Some(outcome),
        judged_at: Some(at),
        ..waiting.clone()
    }
}

/// `waiting` once found at `at` with neither rows nor a record, its re-run no
/// longer in its campaign's plan.
fn dropped(waiting: &PolicyRerun, at: Timestamp) -> PolicyRerun {
    PolicyRerun {
        outcome: Some(RerunOutcome::Dropped),
        judged_at: Some(at),
        ..waiting.clone()
    }
}

/// The rows that lack model params once filled as the plan pages fill them,
/// with no requested model, by the key an entry would take, and how many jobs
/// have one.
fn missing_params(
    table: &ModelParamTable,
    rows: &HashMap<JobId, Vec<ResultRow>>,
) -> (Vec<ModelParamsGap>, usize) {
    let gaps: BTreeMap<String, (usize, BTreeSet<&JobId>)> = rows
        .values()
        .flatten()
        .filter_map(|row| Some((gap_key(table, row)?, &row.job_id)))
        .fold(BTreeMap::new(), |mut gaps, (key, job_id)| {
            let (row_count, jobs) = gaps.entry(key).or_default();
            *row_count += 1;
            jobs.insert(job_id);
            gaps
        });
    let jobs: BTreeSet<&JobId> = gaps.values().flat_map(|(_, jobs)| jobs).copied().collect();
    let missing = gaps
        .into_iter()
        .map(|(key, (row_count, jobs))| ModelParamsGap {
            key,
            row_count,
            job_count: jobs.len(),
        })
        .collect();
    (missing, jobs.len())
}

/// The key an entry would take for `row`'s model, when `row` lacks model params
/// once filled as the plan pages fill it, with no requested model.
fn gap_key(table: &ModelParamTable, row: &ResultRow) -> Option<String> {
    // shortcut: a row that records no model, or whose descriptors give no key,
    // is left out, as its job's requested model would need the plan's cells;
    // read those cells once such rows need an entry too.
    let (total, active) = row.filled_model_params(table, None);
    (total.is_none() || active.is_none())
        .then(|| model_key(row))
        .flatten()
}

/// The recorded models of each job's rows that lack model params, as
/// [`missing_params`] counts them.
fn missing_by_job(
    table: &ModelParamTable,
    rows: &HashMap<JobId, Vec<ResultRow>>,
) -> HashMap<JobId, BTreeSet<RecordedModel>> {
    rows.iter()
        .filter_map(|(job_id, rows)| {
            let models: BTreeSet<RecordedModel> = rows
                .iter()
                .filter(|row| gap_key(table, row).is_some())
                .map(|row| RecordedModel {
                    descriptor: row.model_descriptor.clone(),
                    vault_descriptor: row.model_vault_descriptor.clone(),
                })
                .collect();
            (!models.is_empty()).then(|| (job_id.clone(), models))
        })
        .collect()
}

/// `value` as the console holds it: the API sends an `f32` as its shortest
/// decimal, which the console reads as an `f64`, and a non-finite one as `null`.
fn console(value: Option<f32>) -> Option<f64> {
    value
        .filter(|value| value.is_finite())
        .and_then(|value| value.to_string().parse().ok())
}

/// A row's stddev as a percent of its value, as the console's `spreadPercent`
/// reads it: none on a ratio, a value of 0, or no stddev.
fn spread(row: &ResultRow) -> Option<f64> {
    let (value, stddev) = (console(row.value)?, console(row.value_stddev)?);
    (row.unit != "ratio" && value != 0.0).then(|| (stddev / value).abs() * 100.0)
}

/// The percent spread weighs `row`'s spread against under `settings`: the
/// short repetition's for a row whose repetitions average under its time,
/// else `percent`.
fn limit(settings: &PolicySettings, row: &ResultRow) -> Option<f64> {
    match (
        settings.short_repetition_ms,
        settings.short_repetition_percent,
    ) {
        (Some(ms), Some(short)) if repetition_ms(row).is_some_and(|time| time < ms) => Some(short),
        _ => settings.percent,
    }
}

/// The average time of one of a row's timed repetitions, in milliseconds: a
/// time's value, or a rate's count over its value, as `score.rs` derives the
/// rate from the time. `None` on any other row.
fn repetition_ms(row: &ResultRow) -> Option<f64> {
    let value = console(row.value).filter(|value| *value > 0.0)?;
    if row.unit == "ms" {
        return Some(value);
    }
    let count = match row.metric.as_str() {
        // A VL or ASR prompt's tokens are counted as it runs.
        "prefill_throughput" => row.parameters.get("prefill_tokens").or_else(|| {
            [
                "vl_throughput_prefill_tokens",
                "asr_throughput_prefill_tokens",
            ]
            .iter()
            .find_map(|key| row.observations.get(*key))
        }),
        "decode_throughput" => row.parameters.get("decode_tokens"),
        "decision_throughput" => row.parameters.get("batch_size"),
        _ => None,
    }?;
    Some(count.as_f64()? / value * 1000.0)
}

/// The TTFT check's clocks, as the console's `CLOCKS` lists them: each the time
/// to first token and end-to-end latency metrics a job reports on it.
const CLOCKS: [&[(&str, &str)]; 2] = [
    &[("ttft", "end_to_end_latency"), ("ttft_ms", "e2e_ms")],
    &[("runtime_ttft", "runtime_e2e_ms")],
];

/// A clock's time to first token as a percent of its end-to-end latency, from
/// its first pair whose medians both exist with the latency above 0.
fn share(rows: &[ResultRow], pairs: &[(&str, &str)]) -> Option<f64> {
    pairs.iter().find_map(|(first, end)| {
        let (ttft, e2e) = (median(rows, first)?, median(rows, end)?);
        (e2e > 0.0).then_some(ttft / e2e * 100.0)
    })
}

/// The median of `metric`'s values on rows that did not fail.
fn median(rows: &[ResultRow], metric: &str) -> Option<f64> {
    let mut values: Vec<f64> = rows
        .iter()
        .filter(|row| row.metric == metric && row.failed != Some(true))
        .filter_map(|row| console(row.value))
        .collect();
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    let upper = *values.get(mid)?;
    Some(if values.len() % 2 == 1 {
        upper
    } else {
        (values.get(mid - 1)? + upper) / 2.0
    })
}

/// What [`file()`] did, and what the policy pass keeps of the member after it.
#[derive(Debug)]
pub struct Filing {
    pub outcome: FilingOutcome,
    /// The evaluation's record with the filed plan and any refusal; `None`
    /// when the evaluation had none.
    pub record: Option<GroupPolicyState>,
}

/// How a filing ended. Up to [`Self::Failed`] nothing was written; from
/// [`Self::Rejected`] on the ingest ran.
#[derive(Debug)]
pub enum FilingOutcome {
    /// An acting policy changed since the round read it: nothing is filed
    /// until the next round evaluates the member under the stored policies.
    PoliciesChanged,
    /// The campaign left the group since the round read its members.
    LeftGroup,
    /// A store read before the ingest failed.
    ReadFailed(anyhow::Error),
    /// The campaign moved since the evaluation
    /// ([`CampaignAdminError::Contended`]), or [`Head::read`] refused it now.
    Skipped(CampaignAdminError),
    /// Someone stopped the head since the evaluation.
    Stopped,
    /// [`Head::rerun`] found no document for the head, or one that does not
    /// match its digest, so the head cannot be filed again.
    Refused(CampaignAdminError),
    /// Any other error before the ingest.
    Failed(CampaignAdminError),
    /// The ingest refused the head's document, as for a deadline that has
    /// passed, so the head cannot be filed again.
    Rejected(anyhow::Error),
    /// Any other ingest error.
    IngestFailed(anyhow::Error),
    /// Someone stopped the head during the ingest, so the campaign was not
    /// moved onto `plan_id`, which was cancelled if `cancelled`.
    StoppedDuringFiling { plan_id: PlanId, cancelled: bool },
    /// The campaign runs `plan_id`. `Err` is
    /// [`CampaignAdminError::NotCancelled`]: the plan it replaced still runs.
    Advanced {
        plan_id: PlanId,
        result: Result<Revised, CampaignAdminError>,
    },
    /// The campaign does not run `plan_id`. `Overtaken`, `AdvanceContended`
    /// and `DeletedDuringFiling` say whether the plan was cancelled;
    /// `NotAdvanced` wrote no head, as when the check after the ingest could
    /// not read the head, and its plan is cancelled unless that cancel failed;
    /// and the head write of `CommitUnknown` can still land.
    NotAdvanced {
        plan_id: PlanId,
        error: CampaignAdminError,
    },
}

/// File the revision `evaluation` found for `campaign`, a member of `group`
/// read for the round under its `stored` policies: the head re-run and marked
/// in one plan, put together from `campaign_admin`'s parts as
/// [`campaign_admin::rerun_jobs`] does, with a check that no one stopped the
/// head before the ingest and another after it.
pub async fn file(
    stores: &Stores,
    groups: &dyn CampaignGroupStore,
    settings: IngestSettings<'_>,
    group: &CampaignGroup,
    campaign: &Campaign,
    stored: &[CampaignGroupPolicy],
    evaluation: &Evaluation,
) -> Filing {
    let (outcome, filed) = filing(
        stores, groups, settings, group, campaign, stored, evaluation,
    )
    .await;
    let refused = if let FilingOutcome::Refused(error) = &outcome {
        Some(format!("{error:#}"))
    } else if let FilingOutcome::Rejected(error) = &outcome {
        Some(format!("{error:#}"))
    } else {
        None
    };
    let refusal = evaluation
        .plan_id
        .clone()
        .zip(refused)
        .map(|(plan_id, reason)| Refusal {
            plan_id,
            reason,
            build: BUILD_VERSION.to_owned(),
        });
    let record = evaluation.record.clone().map(|record| GroupPolicyState {
        filed: filed.or(record.filed),
        refusal: refusal.or(record.refusal),
        ..record
    });
    Filing { outcome, record }
}

/// [`file()`]'s steps, and the filing to record once the ingest staged a plan.
async fn filing(
    stores: &Stores,
    groups: &dyn CampaignGroupStore,
    settings: IngestSettings<'_>,
    group: &CampaignGroup,
    campaign: &Campaign,
    stored: &[CampaignGroupPolicy],
    evaluation: &Evaluation,
) -> (FilingOutcome, Option<GroupFiling>) {
    let fresh = match groups.list_policies(&group.group_id).await {
        Ok(fresh) => fresh,
        Err(error) => return (FilingOutcome::ReadFailed(error), None),
    };
    if !same_filers(
        &acting(&group.group_id, &fresh),
        &acting(&group.group_id, stored),
    ) {
        return (FilingOutcome::PoliciesChanged, None);
    }
    match groups.group_of(&campaign.campaign_id).await {
        Ok(of) if of.as_ref() == Some(&group.group_id) => {}
        Ok(_) => return (FilingOutcome::LeftGroup, None),
        Err(error) => return (FilingOutcome::ReadFailed(error), None),
    }
    let Some(base) = &evaluation.plan_id else {
        let none = CampaignAdminError::NoPlan(campaign.campaign_id.clone());
        return (FilingOutcome::Skipped(none), None);
    };
    let head = match head_to_file(stores, campaign, base).await {
        Ok(head) => head,
        Err(outcome) => return (outcome, None),
    };
    let held: HashSet<JobId> = head.manifest().job_ids.iter().cloned().collect();
    let fail = match evaluation
        .marked
        .iter()
        .map(|(job_id, reason)| {
            let reason = NonEmptyTrimmedString::try_new(reason.clone())?;
            anyhow::Ok((job_id.clone(), reason))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()
    {
        Ok(fail) => fail,
        Err(error) => return (FilingOutcome::Failed(error.into()), None),
    };
    let rerun = evaluation.rerun_job_ids.clone();
    let revision = match head
        .rerun(&*stores.plans, &*stores.submissions, rerun, fail)
        .await
    {
        Ok(revision) => revision.by_group(
            group.group_id.clone(),
            evaluation.kinds.iter().copied().collect(),
        ),
        Err(
            error @ (CampaignAdminError::NoDocument(_) | CampaignAdminError::SourceMismatch(_)),
        ) => {
            return (FilingOutcome::Refused(error), None);
        }
        Err(error) => return (FilingOutcome::Failed(error), None),
    };
    let catalog = match crate::org_catalog::load_server_catalog(&*stores.catalog).await {
        Ok(catalog) => catalog,
        Err(error) => return (FilingOutcome::Failed(error.into()), None),
    };
    let membership = match revision.membership(&*stores.plans).await {
        Ok(membership) => membership,
        Err(error) => return (FilingOutcome::Failed(error.into()), None),
    };
    let ctx = IngestContext::with_settings(stores, &catalog, settings);
    let kinds: Vec<&str> = evaluation.kinds.iter().map(|kind| kind.as_ref()).collect();
    let note = format!(
        "Campaign group {} policies {}: re-run {}, marked {} failed.",
        group.group_id,
        kinds.join(", "),
        evaluation.rerun_job_ids.len(),
        evaluation.marked.len(),
    );
    let change = Some(revision.change());
    let input = PlanInput::Text(revision.document());
    let report = match plan_cli::ingest(&ctx, input, Some(note), change, membership).await {
        Ok(report) => report,
        Err(error) if rejected(&error) => return (FilingOutcome::Rejected(error), None),
        Err(error) => return (FilingOutcome::IngestFailed(error), None),
    };
    let filed = GroupFiling {
        plan_id: report.plan_id.clone(),
        queued: report
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id)
            .filter(|job_id| !held.contains(*job_id))
            .cloned()
            .collect(),
        commit_unknown: false,
        filed_at: Timestamp::now(),
    };
    let campaign_id = &campaign.campaign_id;
    let stop = stopped_now(&*stores.plans, base)
        .await
        .with_context(|| format!("reading whether someone stopped plan {base} during the filing"));
    match stop {
        Ok(false) => {}
        Ok(true) => {
            let cancelled = campaign_admin::cancel_unadvanced(
                &*stores.plans,
                campaign_id,
                &report.plan_id,
                "someone stopped the head it revises while it was filed",
            )
            .await;
            let plan_id = report.plan_id;
            return (
                FilingOutcome::StoppedDuringFiling { plan_id, cancelled },
                Some(filed),
            );
        }
        Err(source) => {
            // `complete_revision` never ran, so no head write can land.
            campaign_admin::cancel_unadvanced(
                &*stores.plans,
                campaign_id,
                &report.plan_id,
                "the check that no one stopped the head it revises failed",
            )
            .await;
            let error = CampaignAdminError::NotAdvanced {
                campaign_id: campaign_id.clone(),
                plan_id: report.plan_id.clone(),
                source,
            };
            let plan_id = report.plan_id;
            return (FilingOutcome::NotAdvanced { plan_id, error }, Some(filed));
        }
    }
    let completed = campaign_admin::complete_revision(
        &*stores.campaigns,
        &*stores.plans,
        revision,
        &report.plan_id,
        &report.job_ids,
    )
    .await;
    let plan_id = report.plan_id;
    match completed {
        Ok(revised) => (
            FilingOutcome::Advanced {
                plan_id,
                result: Ok(revised),
            },
            Some(filed),
        ),
        Err(error @ CampaignAdminError::NotCancelled { .. }) => (
            FilingOutcome::Advanced {
                plan_id,
                result: Err(error),
            },
            Some(filed),
        ),
        // `advance` read no campaign to write, or lost the write it tried, so
        // no head write can land.
        Err(error @ CampaignAdminError::NotAdvanced { .. }) => {
            campaign_admin::cancel_unadvanced(
                &*stores.plans,
                campaign_id,
                &plan_id,
                "its campaign could not be read to move it onto the plan",
            )
            .await;
            (FilingOutcome::NotAdvanced { plan_id, error }, Some(filed))
        }
        Err(error) => {
            let commit_unknown = matches!(error, CampaignAdminError::CommitUnknown { .. });
            (
                FilingOutcome::NotAdvanced { plan_id, error },
                Some(GroupFiling {
                    commit_unknown,
                    ..filed
                }),
            )
        }
    }
}

/// The head of `campaign` read again for the filing, refused if the campaign
/// moved since the evaluation read `base` or someone stopped the head.
async fn head_to_file(
    stores: &Stores,
    campaign: &Campaign,
    base: &PlanId,
) -> Result<Head, FilingOutcome> {
    let current = match campaign_admin::require(&*stores.campaigns, &campaign.campaign_id).await {
        Ok(current) => current,
        Err(CampaignAdminError::Store(error)) => return Err(FilingOutcome::ReadFailed(error)),
        Err(error) => return Err(FilingOutcome::Skipped(error)),
    };
    if campaign_admin::changed_under(campaign, &current) {
        let moved = CampaignAdminError::Contended(campaign.campaign_id.clone());
        return Err(FilingOutcome::Skipped(moved));
    }
    let head = match Head::read(&*stores.plans, current, Some(base)).await {
        Ok(head) => head,
        Err(CampaignAdminError::Store(error)) => return Err(FilingOutcome::ReadFailed(error)),
        Err(error) => return Err(FilingOutcome::Skipped(error)),
    };
    let manifest = head.manifest();
    match stopped(&*stores.plans, &manifest.plan_id, manifest.status).await {
        Ok(false) => Ok(head),
        Ok(true) => Err(FilingOutcome::Stopped),
        Err(error) => Err(FilingOutcome::ReadFailed(error)),
    }
}

/// Whether someone stopped `plan_id`, read again.
async fn stopped_now(plans: &dyn PlanStore, plan_id: &PlanId) -> anyhow::Result<bool> {
    let summary = summary(plans, plan_id)
        .await?
        .with_context(|| format!("plan {plan_id} is gone"))?;
    stopped(plans, plan_id, summary.status).await
}

/// Whether the ingest refused the document itself, which a retry does not
/// change: its jobs, or the document no longer parsing or expanding, as after
/// a release changes a rule or a lower `plan_max_jobs`.
fn rejected(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<IngestError>(),
        Some(IngestError::Rejected(_))
    ) || error
        .downcast_ref::<DocumentError>()
        .is_some_and(DocumentError::is_caller_fault)
}

/// How long after the CLI arm's start the policy pass still starts a member:
/// queue-maintenance's cadence, which bounds the reclaim of a dead device's
/// leases.
const BUDGET: Duration = Duration::from_secs(60);

/// queue-maintenance's policy pass, after its queue passes: file what each
/// group's policies that are on find in its members' current plans, one
/// revision per campaign, and keep what they decided and how each re-run they
/// filed answers. No member starts once the run is [`BUDGET`] past `started`,
/// the CLI arm's start. `validate_index` probes the plan index backend before
/// the first filing. An item that fails is logged and skipped, and fails the
/// call once the pass's line is logged. Without a `[database]` there are no
/// groups, and nothing runs.
pub async fn apply<Validate, Validated>(
    config: &Config,
    stores: &Stores,
    started: Instant,
    validate_index: Validate,
) -> anyhow::Result<()>
where
    Validate: Fn() -> Validated + Sync,
    Validated: Future<Output = anyhow::Result<()>> + Send,
{
    let Some(groups) = stores.campaign_groups.as_deref() else {
        return Ok(());
    };
    let begun = Instant::now();
    let counts = round(config, stores, groups, started, &validate_index).await?;
    tracing::info!(
        groups = counts.groups,
        groups_skipped = counts.groups_skipped,
        members = counts.members,
        members_read = counts.members_read,
        members_not_reached = counts.members_not_reached,
        members_unreadable = counts.members_unreadable,
        members_skipped = counts.members_skipped,
        revisions = counts.revisions,
        rerun_jobs = counts.rerun_jobs,
        marked_jobs = counts.marked_jobs,
        reruns_recorded = counts.reruns_recorded,
        reruns_judged = counts.reruns_judged,
        jobs_waiting_for_rows = counts.jobs_waiting_for_rows,
        body_reads = counts.body_reads,
        body_reruns = counts.body_reruns,
        members_waiting_for_devices = counts.members_waiting_for_devices,
        filings_not_advanced = counts.filings_not_advanced,
        cancel_markers_written = counts.cancel_markers_written,
        filings_stopped = counts.filings_stopped,
        refused_heads = counts.refused_heads,
        other_owner_members = counts.other_owner_members,
        jobs_left_for_good = counts.jobs_left_for_good,
        models_missing_params = counts.models_missing_params.len(),
        failed_items = counts.failed_items,
        duration_ms = millis(begun.elapsed()),
        run_duration_ms = millis(started.elapsed()),
        "queue-maintenance: policy pass finished"
    );
    if counts.failed_items > 0 {
        anyhow::bail!(
            "queue-maintenance: {} policy pass item(s) failed and were skipped (see warnings)",
            counts.failed_items
        );
    }
    Ok(())
}

/// What a round of the policy pass did: the fields of its log line.
#[derive(Debug, Default, PartialEq, Eq)]
struct PassCounts {
    /// Groups with a policy on, past those skipped for a policy.
    groups: usize,
    /// Groups with a policy that does not decode or is on with settings its
    /// kind refuses (failed without codes), and groups whose record or members
    /// did not read.
    groups_skipped: usize,
    members: usize,
    members_read: usize,
    /// Members the budget left to the next round.
    members_not_reached: usize,
    /// Members whose rows pipette-duckdb did not answer, or that an earlier
    /// member's failed read left unread.
    members_unreadable: usize,
    /// Members whose kept state does not decode, or whose campaign, plans or
    /// waiting re-runs did not read.
    members_skipped: usize,
    /// Filings the campaign moved onto, and the jobs they re-ran and marked.
    revisions: usize,
    rerun_jobs: usize,
    marked_jobs: usize,
    /// The re-runs recorded for those filings, one per policy that re-ran a
    /// job, and the recorded ones whose outcome this round wrote.
    reruns_recorded: usize,
    reruns_judged: usize,
    jobs_waiting_for_rows: usize,
    /// The submission bodies read for answered jobs with no rows, and the jobs
    /// of advanced filings that their bodies' rows re-ran.
    body_reads: usize,
    body_reruns: usize,
    members_waiting_for_devices: usize,
    /// This round's filings the campaign did not move onto, and earlier ones
    /// that still run and hold their campaign.
    filings_not_advanced: usize,
    /// The cancel markers this round wrote on those earlier ones.
    cancel_markers_written: usize,
    /// Filings whose head someone stopped during the ingest.
    filings_stopped: usize,
    /// Heads refused this round, and heads an earlier refusal holds.
    refused_heads: usize,
    other_owner_members: usize,
    jobs_left_for_good: usize,
    /// The keys that the rows read lack an entry under.
    models_missing_params: BTreeSet<String>,
    failed_items: usize,
}

/// One round of the policy pass over every group with a policy on.
async fn round<Validate, Validated>(
    config: &Config,
    stores: &Stores,
    groups: &dyn CampaignGroupStore,
    started: Instant,
    validate_index: &Validate,
) -> anyhow::Result<PassCounts>
where
    Validate: Fn() -> Validated + Sync,
    Validated: Future<Output = anyhow::Result<()>> + Send,
{
    let enabled = groups.enabled_policies().await?;
    enabled.unreadable.iter().for_each(|(group_id, bad)| {
        tracing::warn!(
            %group_id,
            key = %bad.key,
            reason = %bad.reason,
            "queue-maintenance: unreadable campaign group policy, its group skipped"
        );
    });
    let unreadable: BTreeSet<&CampaignGroupId> = enabled
        .unreadable
        .iter()
        .map(|(group_id, _)| group_id)
        .collect();
    let on: Vec<&[CampaignGroupPolicy]> = enabled
        .policies
        .chunk_by(|a, b| a.group_id == b.group_id)
        .filter(|policies| {
            policies
                .first()
                .is_some_and(|policy| !unreadable.contains(&policy.group_id))
        })
        .collect();
    let counts = PassCounts {
        groups: on.len(),
        groups_skipped: unreadable.len(),
        failed_items: unreadable.len(),
        ..PassCounts::default()
    };
    if on.is_empty() {
        return Ok(counts);
    }
    let Some(duckdb) = config.duckdb.clone() else {
        tracing::warn!(
            groups = on.len(),
            "queue-maintenance: campaign group policies are on, and the policy pass reads their \
             results from pipette-duckdb: add a [duckdb] section to the config"
        );
        return Ok(PassCounts {
            failed_items: counts.failed_items + 1,
            ..counts
        });
    };
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.http_connect_timeout_secs.get()))
        .build()?;
    let results = ResultsDb::new(http, duckdb)?;
    let (read, unread_groups) = futures::stream::iter(on)
        .fold(
            (Vec::new(), 0_usize),
            |(mut read, unread), stored| async move {
                let Some(group_id) = stored.first().map(|policy| &policy.group_id) else {
                    return (read, unread);
                };
                match members(groups, group_id).await {
                    Ok(Some((group, members))) => {
                        read.push((group, stored, members));
                        (read, unread)
                    }
                    Ok(None) => (read, unread),
                    Err(error) => {
                        tracing::warn!(
                            %group_id,
                            error = %format!("{error:#}"),
                            "queue-maintenance: failed to read a campaign group or its members, \
                             skipped this round"
                        );
                        (read, unread + 1)
                    }
                }
            },
        )
        .await;
    let mut members: Vec<(&CampaignGroup, &[CampaignGroupPolicy], &GroupMember)> = read
        .iter()
        .flat_map(|(group, stored, members)| {
            members.iter().map(move |member| (group, *stored, member))
        })
        .collect();
    members.sort_by(|(_, _, a), (_, _, b)| order(a).cmp(&order(b)));
    let counts = PassCounts {
        groups_skipped: counts.groups_skipped + unread_groups,
        members: members.len(),
        failed_items: counts.failed_items + unread_groups,
        ..counts
    };
    let pass = &Round {
        config,
        stores,
        groups,
        table: OnceCell::new(),
        catalog: OnceCell::new(),
        probed: OnceCell::new(),
        validate_index,
    };
    let (counts, _) = futures::stream::iter(members)
        .fold(
            (counts, Some(&results)),
            |(mut counts, results), (group, stored, member)| async move {
                if started.elapsed() > BUDGET {
                    counts.members_not_reached += 1;
                    return (counts, results);
                }
                counts.members_read += 1;
                let taken = pass.take(results, group, stored, member, &mut counts).await;
                // A failed pipette-duckdb read ends the round's reads.
                let unreadable = taken.is_some_and(|(evaluation, _)| {
                    evaluation.state == PolicyMemberState::Unreadable
                });
                let results = results.filter(|_| !unreadable);
                (counts, results)
            },
        )
        .await;
    Ok(counts)
}

/// The group `group_id` and its members; `None` if it is gone.
async fn members(
    groups: &dyn CampaignGroupStore,
    group_id: &CampaignGroupId,
) -> anyhow::Result<Option<(CampaignGroup, Vec<GroupMember>)>> {
    let Some(group) = groups.get_group(group_id).await? else {
        return Ok(None);
    };
    Ok(Some((group, groups.policy_states(group_id).await?)))
}

/// A round takes members by when the pass last filed into each or failed to
/// read its rows, longest ago first and those with neither before any, so one
/// member's ingests cannot use up every round's budget, nor one member's failing
/// read end every round's reads.
fn order(member: &GroupMember) -> (Option<Timestamp>, &CampaignId) {
    let last = member
        .policy_state
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(|state| {
            let filed_at = state.filed.as_ref().map(|filed| filed.filed_at);
            filed_at.max(state.unreadable_at)
        });
    (last, &member.campaign_id)
}

/// What every member of a round is taken with.
struct Round<'a, Validate> {
    config: &'a Config,
    stores: &'a Stores,
    groups: &'a dyn CampaignGroupStore,
    /// The model params table of the round's filings and model_params checks,
    /// loaded for the first.
    table: OnceCell<ModelParamTable>,
    /// The benchmark catalog of the round's smaller_failed checks, loaded for
    /// the first.
    catalog: OnceCell<HashMap<BenchmarkId, Benchmark>>,
    /// Set once the plan index backend passed its probe, before the round's
    /// first filing.
    probed: OnceCell<()>,
    validate_index: &'a Validate,
}

impl<Validate, Validated> Round<'_, Validate>
where
    Validate: Fn() -> Validated + Sync,
    Validated: Future<Output = anyhow::Result<()>> + Send,
{
    /// Evaluate `member` of `group`, whose policies that are on are `stored`,
    /// then cancel the filing that holds it, file what it finds, or keep what
    /// it decided. `None` when the member was skipped before its evaluation.
    async fn take(
        &self,
        results: Option<&ResultsDb>,
        group: &CampaignGroup,
        stored: &[CampaignGroupPolicy],
        member: &GroupMember,
        counts: &mut PassCounts,
    ) -> Option<(Evaluation, Option<Filing>)> {
        let campaign_id = &member.campaign_id;
        let kept = match &member.policy_state {
            Ok(kept) => kept.as_ref(),
            Err(bad) => {
                tracing::warn!(
                    %campaign_id,
                    key = %bad.key,
                    reason = %bad.reason,
                    "queue-maintenance: unreadable campaign group member state, skipped"
                );
                counts.members_skipped += 1;
                counts.failed_items += 1;
                return None;
            }
        };
        let campaign = match self.stores.campaigns.get_campaign(campaign_id).await {
            Ok(Some(campaign)) => campaign,
            // Deleted since the round read the members, which took its row.
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to read a campaign group member, skipped this round"
                );
                counts.members_skipped += 1;
                counts.failed_items += 1;
                return None;
            }
        };
        let checks_params = stored
            .iter()
            .any(|policy| policy.kind == PolicyKind::ModelParams);
        let table = if checks_params {
            match self.table().await {
                Ok(table) => Some(table),
                Err(error) => {
                    tracing::warn!(
                        %campaign_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: failed to read the model params table for a campaign \
                         group member, skipped this round"
                    );
                    counts.members_skipped += 1;
                    counts.failed_items += 1;
                    return None;
                }
            }
        } else {
            None
        };
        let waiting = match self
            .groups
            .waiting_reruns(&group.group_id, campaign_id)
            .await
        {
            Ok(waiting) => waiting,
            Err(error) => {
                tracing::warn!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to read a campaign group member's waiting re-runs, \
                     skipped this round"
                );
                counts.members_skipped += 1;
                counts.failed_items += 1;
                return None;
            }
        };
        let checks_families = stored
            .iter()
            .any(|policy| policy.kind == PolicyKind::SmallerFailed);
        let catalog = if checks_families {
            match self.catalog().await {
                Ok(catalog) => Some(catalog),
                Err(error) => {
                    tracing::warn!(
                        %campaign_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: failed to read the benchmark catalog for a campaign \
                         group member, skipped this round"
                    );
                    counts.members_skipped += 1;
                    counts.failed_items += 1;
                    return None;
                }
            }
        } else {
            None
        };
        let stored_job_states = match self.groups.job_states(&group.group_id, campaign_id).await {
            Ok(job_states) => job_states,
            Err(error) => {
                tracing::warn!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to read a campaign group member's job states, \
                     skipped this round"
                );
                counts.members_skipped += 1;
                counts.failed_items += 1;
                return None;
            }
        };
        let reruns = stored.iter().any(|policy| policy.kind.reruns_or_marks());
        let bodies = if reruns {
            match self.catalog().await {
                Ok(catalog) => Some(BodyReads {
                    catalog,
                    stores: self.stores,
                }),
                Err(error) => {
                    tracing::warn!(
                        %campaign_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: failed to read the benchmark catalog, so a campaign \
                         group member's jobs wait for their rows"
                    );
                    None
                }
            }
        } else {
            None
        };
        let state = kept.cloned().unwrap_or_default();
        let pass = Pass {
            state: &state,
            todo: &*self.stores.todo,
            submissions: &*self.stores.submissions,
            preview: false,
            table,
            catalog,
            waiting: &waiting,
            job_states: &stored_job_states,
            bodies,
        };
        let plans = &*self.stores.plans;
        let evaluation = match evaluate(plans, results, group, &campaign, stored, pass).await {
            Ok(evaluation) => evaluation,
            Err(error) => {
                tracing::warn!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to read a campaign group member's plans, skipped \
                     this round"
                );
                counts.members_skipped += 1;
                counts.failed_items += 1;
                return None;
            }
        };
        counts.evaluated(campaign_id, &evaluation);
        if let Some(Hold::NotAdvanced {
            plan_id,
            commit_unknown: false,
        }) = &evaluation.hold
        {
            // `cancel_unadvanced` logs the cancel either way.
            let cancelled = campaign_admin::cancel_unadvanced(
                plans,
                campaign_id,
                plan_id,
                "a campaign group filing did not advance and still ran",
            )
            .await;
            if cancelled {
                counts.cancel_markers_written += 1;
            } else {
                counts.failed_items += 1;
            }
        }
        // A judged re-run is not read again once the state keeps it as
        // decided, so nothing is filed or kept before its outcome's write.
        if !self
            .write_outcomes(group, campaign_id, &evaluation, counts)
            .await
        {
            return Some((evaluation, None));
        }
        let finds = !(evaluation.rerun_job_ids.is_empty() && evaluation.marked.is_empty());
        let filing = if evaluation.state == PolicyMemberState::Evaluated && finds {
            self.revise(group, stored, &campaign, &evaluation, counts)
                .await
        } else {
            None
        };
        let record = match &filing {
            Some(filing) => filing.record.as_ref(),
            None => evaluation.record.as_ref(),
        };
        let changes = evaluation
            .job_states
            .as_deref()
            .map(|job_states| job_state_changes(&stored_job_states, job_states))
            .unwrap_or_default();
        let changed = changes != JobStateChanges::default();
        if let Some(record) = record.filter(|&record| changed || kept != Some(record)) {
            match self
                .groups
                .set_policy_state(&group.group_id, campaign_id, record, &changes)
                .await
            {
                Ok(true) => {}
                Ok(false) => tracing::info!(
                    %campaign_id,
                    "queue-maintenance: a member left its campaign group during the policy pass; \
                     its state is not kept"
                ),
                Err(error) => {
                    tracing::warn!(
                        %campaign_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: failed to keep a campaign group member's state"
                    );
                    counts.failed_items += 1;
                }
            }
        }
        if let Some(FilingOutcome::Advanced { plan_id, .. }) =
            filing.as_ref().map(|filing| &filing.outcome)
        {
            counts.body_reruns += evaluation.from_body.len();
            evaluation.from_body.iter().for_each(|job_id| {
                tracing::info!(
                    group_id = %group.group_id,
                    %campaign_id,
                    %job_id,
                    filed = %plan_id,
                    "queue-maintenance: a campaign group filing re-runs a job on the rows of its \
                     submission body"
                );
            });
            self.write_reruns(group, campaign_id, &evaluation, plan_id, counts)
                .await;
        }
        Some((evaluation, filing))
    }

    /// Write the outcomes `evaluation` judged; `false` when the write failed.
    async fn write_outcomes(
        &self,
        group: &CampaignGroup,
        campaign_id: &CampaignId,
        evaluation: &Evaluation,
        counts: &mut PassCounts,
    ) -> bool {
        if evaluation.judged.is_empty() {
            return true;
        }
        match self
            .groups
            .judge_reruns(&group.group_id, &evaluation.judged)
            .await
        {
            Ok(judged) => {
                counts.reruns_judged += judged;
                true
            }
            Err(error) => {
                tracing::warn!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to record how a campaign group member's re-runs \
                     answered"
                );
                counts.failed_items += 1;
                false
            }
        }
    }

    /// Record each re-run of `evaluation` that the filed `plan_id` holds,
    /// waiting for its rows. The plan's manifest names the job each re-runs.
    async fn write_reruns(
        &self,
        group: &CampaignGroup,
        campaign_id: &CampaignId,
        evaluation: &Evaluation,
        plan_id: &PlanId,
        counts: &mut PassCounts,
    ) {
        let recorded = async {
            let filed = self
                .stores
                .plans
                .get_plan(plan_id)
                .await?
                .with_context(|| format!("plan {plan_id} is gone"))?;
            let reruns: Vec<PolicyRerun> = filed
                .rerun_of
                .iter()
                .filter_map(|(job, of)| Some((job, of, evaluation.reruns.get(of)?)))
                .flat_map(|(job, of, reruns)| {
                    reruns
                        .iter()
                        .map(|rerun| rerun.waiting(campaign_id, &filed, job, of))
                })
                .collect();
            self.groups.record_reruns(&group.group_id, &reruns).await
        };
        match recorded.await {
            Ok(recorded) => counts.reruns_recorded += recorded,
            Err(error) => {
                tracing::warn!(
                    %campaign_id,
                    filed = %plan_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to record the re-runs a campaign group filing \
                     queued"
                );
                counts.failed_items += 1;
            }
        }
    }

    /// File what `evaluation` found in `campaign`, then log, count and audit
    /// how the filing ended; `None` when the round could not prepare it.
    async fn revise(
        &self,
        group: &CampaignGroup,
        stored: &[CampaignGroupPolicy],
        campaign: &Campaign,
        evaluation: &Evaluation,
        counts: &mut PassCounts,
    ) -> Option<Filing> {
        let settings = match self.settings().await {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(
                    campaign_id = %campaign.campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: failed to prepare a campaign group filing, skipped this \
                     round"
                );
                counts.failed_items += 1;
                return None;
            }
        };
        let filing = file(
            self.stores,
            self.groups,
            settings,
            group,
            campaign,
            stored,
            evaluation,
        )
        .await;
        if let Some((outcome, filed)) =
            counts.filed(&campaign.campaign_id, evaluation, &filing.outcome)
        {
            self.audit(group, stored, campaign, evaluation, filed, outcome)
                .await;
        }
        Some(filing)
    }

    /// The round's model params table, read again on the next call if the read
    /// failed.
    async fn table(&self) -> anyhow::Result<&ModelParamTable> {
        self.table
            .get_or_try_init(|| ModelParamTable::load(&*self.stores.model_params))
            .await
    }

    /// The round's benchmark catalog, read again on the next call if the read
    /// failed.
    async fn catalog(&self) -> anyhow::Result<&HashMap<BenchmarkId, Benchmark>> {
        self.catalog
            .get_or_try_init(|| crate::org_catalog::load_server_catalog(&*self.stores.catalog))
            .await
    }

    /// The ingest settings of the round's filings, as the CLI's re-run builds
    /// them. The plan index backend is probed before the first, and again
    /// before the next if the probe failed.
    async fn settings(&self) -> anyhow::Result<IngestSettings<'_>> {
        self.probed
            .get_or_try_init(|| (self.validate_index)())
            .await?;
        let table = self.table().await?;
        Ok(IngestSettings {
            descriptor_config: &self.config.descriptor_catalog,
            models: ModelCheck::warn(table),
            max_jobs: self.config.plan_max_jobs,
            creating_max_age: Duration::from_secs(self.config.plan_creating_max_age_secs.get()),
            // A re-run files the head's stored document, which imports nothing.
            import: None,
        })
    }

    /// Record a filing that reached the ingest, under the host: `outcome`, and
    /// `filed`, the plan it filed if any.
    async fn audit(
        &self,
        group: &CampaignGroup,
        stored: &[CampaignGroupPolicy],
        campaign: &Campaign,
        evaluation: &Evaluation,
        filed: Option<&PlanId>,
        outcome: AuditOutcome,
    ) {
        let Some(log) = self.stores.audit.as_deref() else {
            return;
        };
        let plans = evaluation.plan_id.iter().chain(filed);
        let targets: BTreeSet<AuditTarget> = [
            AuditTarget::Campaign {
                campaign_id: campaign.campaign_id.clone(),
            },
            AuditTarget::CampaignGroup {
                group_id: group.group_id.clone(),
            },
            AuditTarget::from(group.owner.clone()),
        ]
        .into_iter()
        .chain(plans.map(|plan_id| AuditTarget::Plan {
            plan_id: plan_id.clone(),
        }))
        .chain(
            stored
                .iter()
                .filter(|policy| policy.kind.reruns_or_marks())
                .filter_map(|policy| policy.enabled_by.clone())
                .map(|user_id| AuditTarget::User { user_id }),
        )
        .collect();
        let action = AuditAction::GroupPolicy {
            group_id: group.group_id.clone(),
            kinds: evaluation.kinds.iter().copied().collect(),
            rerun_job_count: evaluation.rerun_job_ids.len(),
            marked_job_count: evaluation.marked.len(),
        };
        let mut event = new_event(host_actor(), action, outcome);
        let reach = AuditReach {
            orgs: &*self.stores.orgs,
            campaigns: &*self.stores.campaigns,
            campaign_groups: self.stores.campaign_groups.as_deref(),
            plans: &*self.stores.plans,
            vault: &self.stores.vault,
        };
        event.org_ids = reach.orgs_of(&targets).await;
        event.targets = targets;
        if let Err(error) = log.append(&event).await {
            tracing::error!(
                event_id = %event.event_id,
                error = %format!("{error:#}"),
                "failed to write the audit event"
            );
        }
    }
}

impl PassCounts {
    /// Count what the evaluation of `campaign_id` found, and log what holds it.
    fn evaluated(&mut self, campaign_id: &CampaignId, evaluation: &Evaluation) {
        let plan_id = evaluation.plan_id.as_ref().map(tracing::field::display);
        match &evaluation.hold {
            None => {}
            Some(Hold::Queued {
                plan_id: filed,
                waiting,
            }) => {
                self.members_waiting_for_devices += 1;
                tracing::info!(
                    %campaign_id,
                    %filed,
                    waiting,
                    "queue-maintenance: the jobs a campaign group filing queued wait for devices, \
                     so its campaign is not read"
                );
            }
            Some(Hold::NotAdvanced {
                plan_id: filed,
                commit_unknown,
            }) => {
                self.filings_not_advanced += 1;
                if *commit_unknown {
                    tracing::warn!(
                        %campaign_id,
                        %filed,
                        "queue-maintenance: a campaign group filing whose head write is unknown \
                         still runs; cancel it, or confirm the campaign runs it"
                    );
                    self.failed_items += 1;
                }
            }
            Some(Hold::Refused(reason)) => {
                self.refused_heads += 1;
                tracing::warn!(
                    %campaign_id,
                    plan_id,
                    %reason,
                    "queue-maintenance: the campaign group policies cannot file into this plan \
                     again; it is read again once it or the policies change, or after a new \
                     release"
                );
            }
        }
        match evaluation.state {
            PolicyMemberState::Evaluated => {
                self.jobs_waiting_for_rows += evaluation.waiting_job_count;
                self.body_reads += evaluation.body_reads;
                self.jobs_left_for_good += evaluation
                    .skipped
                    .values()
                    .filter(|reason| for_good(**reason))
                    .count();
                evaluation.missing_model_params.iter().for_each(|gap| {
                    tracing::info!(
                        %campaign_id,
                        key = %gap.key,
                        rows = gap.row_count,
                        jobs = gap.job_count,
                        "queue-maintenance: result rows lack model params"
                    );
                    self.models_missing_params.insert(gap.key.clone());
                });
            }
            PolicyMemberState::OtherOwner => {
                self.other_owner_members += 1;
                tracing::warn!(
                    %campaign_id,
                    "queue-maintenance: a member campaign has another owner than its group, so \
                     the group's policies leave it as it is"
                );
            }
            // `evaluate` logs the read that failed.
            PolicyMemberState::Unreadable => {
                self.members_unreadable += 1;
                self.failed_items += 1;
            }
            PolicyMemberState::NoPlan
            | PolicyMemberState::NotReady
            | PolicyMemberState::Cancelled
            | PolicyMemberState::WaitingForDevices
            | PolicyMemberState::FilingNotAdvanced
            | PolicyMemberState::Refused => {}
        }
    }

    /// Count and log how a filing into `campaign_id` ended. From the ingest
    /// on, the outcome of its audit event and the plan it filed, if one.
    fn filed<'o>(
        &mut self,
        campaign_id: &CampaignId,
        evaluation: &Evaluation,
        outcome: &'o FilingOutcome,
    ) -> Option<(AuditOutcome, Option<&'o PlanId>)> {
        let head = evaluation.plan_id.as_ref().map(tracing::field::display);
        match outcome {
            FilingOutcome::PoliciesChanged => {
                tracing::info!(
                    %campaign_id,
                    "queue-maintenance: a campaign group policy changed since the round read it; \
                     nothing was filed"
                );
                None
            }
            FilingOutcome::LeftGroup => {
                tracing::info!(
                    %campaign_id,
                    "queue-maintenance: a campaign left its group since the round read the members; \
                     nothing was filed"
                );
                None
            }
            FilingOutcome::ReadFailed(error) => {
                tracing::warn!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: a read before a campaign group filing failed; nothing was \
                     filed"
                );
                self.members_skipped += 1;
                self.failed_items += 1;
                None
            }
            FilingOutcome::Skipped(error) => {
                tracing::info!(
                    %campaign_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: a campaign changed since its policies read it; nothing \
                     was filed"
                );
                None
            }
            FilingOutcome::Stopped => {
                tracing::info!(
                    %campaign_id,
                    plan_id = head,
                    "queue-maintenance: someone stopped the plan a campaign group filing revises; \
                     nothing was filed"
                );
                None
            }
            FilingOutcome::Refused(error) => {
                self.refused_heads += 1;
                self.failed_items += 1;
                tracing::warn!(
                    %campaign_id,
                    plan_id = head,
                    error = %format!("{error:#}"),
                    "queue-maintenance: a campaign group filing cannot file the plan's document \
                     again; the plan is not read again under these policies until a new release"
                );
                None
            }
            FilingOutcome::Failed(error) => {
                self.failed_items += 1;
                tracing::warn!(
                    %campaign_id,
                    plan_id = head,
                    error = %format!("{error:#}"),
                    "queue-maintenance: a campaign group filing failed before its ingest"
                );
                None
            }
            FilingOutcome::Rejected(error) => {
                self.refused_heads += 1;
                self.failed_items += 1;
                tracing::warn!(
                    %campaign_id,
                    plan_id = head,
                    error = %format!("{error:#}"),
                    "queue-maintenance: the ingest refused a campaign group filing's document; the \
                     plan is not read again under these policies until a new release"
                );
                Some((AuditOutcome::Failed, None))
            }
            FilingOutcome::IngestFailed(error) => {
                self.failed_items += 1;
                tracing::warn!(
                    %campaign_id,
                    plan_id = head,
                    error = %format!("{error:#}"),
                    "queue-maintenance: a campaign group filing's ingest failed"
                );
                Some((AuditOutcome::Failed, None))
            }
            FilingOutcome::StoppedDuringFiling { plan_id, cancelled } => {
                self.filings_stopped += 1;
                if *cancelled {
                    tracing::info!(
                        %campaign_id,
                        plan_id = head,
                        filed = %plan_id,
                        "queue-maintenance: someone stopped the plan a campaign group filing \
                         revises during its ingest; the filed plan is cancelled"
                    );
                } else {
                    // `cancel_unadvanced` logged the failed cancel, and the next
                    // round's hold tries again.
                    self.failed_items += 1;
                }
                Some((AuditOutcome::Failed, Some(plan_id)))
            }
            FilingOutcome::Advanced { plan_id, result } => {
                self.revisions += 1;
                self.rerun_jobs += evaluation.rerun_job_ids.len();
                self.marked_jobs += evaluation.marked.len();
                if let Err(error) = result {
                    self.failed_items += 1;
                    tracing::warn!(
                        %campaign_id,
                        filed = %plan_id,
                        error = %format!("{error:#}"),
                        "queue-maintenance: a campaign group filing advanced, and the plan it \
                         replaced still runs"
                    );
                }
                Some((AuditOutcome::Succeeded, Some(plan_id)))
            }
            FilingOutcome::NotAdvanced { plan_id, error } => {
                self.filings_not_advanced += 1;
                let cancelled = matches!(
                    error,
                    CampaignAdminError::Overtaken {
                        cancelled: true,
                        ..
                    } | CampaignAdminError::AdvanceContended {
                        cancelled: true,
                        ..
                    } | CampaignAdminError::DeletedDuringFiling {
                        cancelled: true,
                        ..
                    }
                );
                if !cancelled {
                    self.failed_items += 1;
                }
                tracing::warn!(
                    %campaign_id,
                    filed = %plan_id,
                    error = %format!("{error:#}"),
                    "queue-maintenance: a campaign group filing did not advance"
                );
                Some((AuditOutcome::Failed, Some(plan_id)))
            }
        }
    }
}

/// What [`backfill`] read and recorded.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Backfilled {
    /// The group filings read.
    pub filings: usize,
    /// The rows those filings give.
    pub found: usize,
    /// The rows written now; the others were recorded already.
    pub recorded: usize,
}

/// Record the re-runs each of `group_ids`' policies filed before the policy
/// pass recorded them. In each member campaign's history, each plan the group
/// filed gives a row per job of a performance benchmark it re-ran
/// ([`performance_jobs`]) and per policy its change names that flags the run
/// it replaces with a re-run left, the policy as stored now. A
/// row's `attempt` counts the runs in a row that the policy flags, back from
/// the replaced run along the group's re-runs, and the policy has a re-run
/// left while that is at most its `max_reruns`. A re-run whose rows landed is
/// judged on them, one with neither rows nor a record that the campaign's
/// current plan no longer holds is dropped, and the pass judges the others. A
/// row recorded already is kept, so a second run records nothing.
pub async fn backfill(
    stores: &Stores,
    results: &ResultsDb,
    group_ids: &[CampaignGroupId],
) -> anyhow::Result<Backfilled> {
    let groups = stores
        .campaign_groups
        .as_deref()
        .context("campaign groups are kept only on a [database]")?;
    futures::stream::iter(group_ids)
        .map(Ok)
        .try_fold(Backfilled::default(), |done, group_id| async move {
            anyhow::ensure!(
                groups.get_group(group_id).await?.is_some(),
                "no campaign group {group_id}"
            );
            let stored = &groups.list_policies(group_id).await?;
            futures::stream::iter(groups.members(group_id).await?)
                .map(Ok)
                .try_fold(done, |done, campaign_id| async move {
                    let (filings, reruns) =
                        filed_reruns(stores, results, group_id, stored, &campaign_id).await?;
                    let recorded = groups.record_reruns(group_id, &reruns).await?;
                    anyhow::Ok(Backfilled {
                        filings: done.filings + filings,
                        found: done.found + reruns.len(),
                        recorded: done.recorded + recorded,
                    })
                })
                .await
        })
        .await
}

/// A group's filing, the policies its change names, and each job it minted to
/// run another again, `(job, the one it runs again)`, its marks among them.
type Filed<'p> = (
    &'p PlanManifest,
    &'p [PolicyKind],
    Vec<(&'p JobId, &'p JobId)>,
);

/// The number of `group_id`'s filings in the history of `campaign_id`, a
/// member whose group's policies are `stored`, and the rows they give.
async fn filed_reruns(
    stores: &Stores,
    results: &ResultsDb,
    group_id: &CampaignGroupId,
    stored: &[CampaignGroupPolicy],
    campaign_id: &CampaignId,
) -> anyhow::Result<(usize, Vec<PolicyRerun>)> {
    let Some(campaign) = stores.campaigns.get_campaign(campaign_id).await? else {
        return Ok((0, Vec::new()));
    };
    // A filing that did not advance is no link of the history.
    let history = crate::campaign_chain::chain_manifests(&*stores.plans, &campaign).await?;
    let ids: Vec<PlanId> = history.iter().map(|plan| plan.plan_id.clone()).collect();
    let notes = stores.plans.get_plan_notes_of(&ids).await?;
    let filings: Vec<Filed<'_>> = history
        .windows(2)
        .filter_map(|pair| {
            let [filed, revised] = pair else {
                return None;
            };
            let Some(RevisionChange::Rerun {
                group_id: Some(by),
                policies,
                ..
            }) = notes.get(&filed.plan_id)?.change.as_ref()
            else {
                return None;
            };
            let held: HashSet<&JobId> = revised.job_ids.iter().collect();
            let minted = filed
                .rerun_of
                .iter()
                .filter(|(job, _)| !held.contains(job))
                .collect();
            (by == group_id).then_some((filed, policies.as_slice(), minted))
        })
        .collect();
    let filings: Vec<Filed<'_>> = futures::stream::iter(filings)
        .then(|(filed, policies, minted)| async move {
            let jobs = minted.iter().map(|(job, _)| (*job).clone()).collect();
            let judged: HashSet<JobId> =
                performance_jobs(filed, &Sources::new(&*stores.plans, jobs))
                    .await?
                    .into_iter()
                    .collect();
            let minted = minted
                .into_iter()
                .filter(|(job, _)| judged.contains(*job))
                .collect();
            anyhow::Ok((filed, policies, minted))
        })
        .try_collect()
        .await?;
    let minted: Vec<(&JobId, &JobId)> = filings
        .iter()
        .flat_map(|(_, _, jobs)| jobs.iter().copied())
        .collect();
    let jobs: Vec<JobId> = minted
        .iter()
        .flat_map(|(job, of)| [*job, *of])
        .cloned()
        .collect::<BTreeSet<JobId>>()
        .into_iter()
        .collect();
    let read = &rows_by_job(results.all_job_results(&jobs).await?);
    let submissions = &*stores.submissions;
    // Each minted job and, if it has rows or a record, whether it is a mark: a
    // mark's rows name it, or its record before they land.
    let answered: Vec<(&JobId, Option<bool>)> =
        futures::stream::iter(minted.iter().map(|(job, _)| *job))
            .map(|job| async move {
                let marked = match read.get(job) {
                    Some(rows) => Some(
                        rows.iter()
                            .any(|row| row.failure_source == Some(FailureSource::Marked)),
                    ),
                    None => submissions.find_job(job).await?.map(|record| {
                        record.body.get("failure_source")
                            == Some(&serde_json::json!(FailureSource::Marked))
                    }),
                };
                anyhow::Ok((job, marked))
            })
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_collect()
            .await?;
    let marks: HashSet<&JobId> = answered
        .iter()
        .filter(|(_, marked)| *marked == Some(true))
        .map(|(job, _)| *job)
        .collect();
    let unrecorded: HashSet<&JobId> = answered
        .iter()
        .filter(|(_, marked)| marked.is_none())
        .map(|(job, _)| *job)
        .collect();
    let reruns: HashMap<&JobId, &JobId> = minted
        .into_iter()
        .filter(|(job, _)| !marks.contains(job))
        .collect();
    let named = filings.iter().flat_map(|(filed, policies, jobs)| {
        jobs.iter()
            .filter(|(job, _)| reruns.contains_key(job))
            .flat_map(move |(job, of)| policies.iter().map(move |kind| (*filed, *kind, *job, *of)))
    });
    // The pass's attempts: the runs in a row that `policy` flags, back from
    // `of` along the group's re-runs.
    let attempt = |policy: &CampaignGroupPolicy, of: &JobId| {
        std::iter::successors(Some(of), |run| reruns.get(run).copied())
            .take_while(|run| read.get(*run).is_some_and(|rows| flags(policy, rows)))
            .count()
    };
    let current: HashSet<&JobId> = history
        .first()
        .into_iter()
        .flat_map(|head| &head.job_ids)
        .collect();
    let at = Timestamp::now();
    let rows = named
        .map(|(filed, kind, job, of)| {
            let policy = kind.policy(group_id, stored);
            let attempt = u32::try_from(attempt(&policy, of))?;
            // The pass records only the policies that flag the run with a
            // re-run left.
            let rerunning = read
                .get(of)
                .filter(|before| (1..=rerun_limit(&policy, before)).contains(&attempt))
                .and_then(|before| Rerunning::new(&policy, attempt, before));
            anyhow::Ok(rerunning.map(|rerunning| {
                let waiting = rerunning.waiting(campaign_id, filed, job, of);
                match read.get(job) {
                    Some(after) => judged(group_id, &waiting, after, at),
                    None if !current.contains(job) && unrecorded.contains(job) => {
                        dropped(&waiting, at)
                    }
                    None => waiting,
                }
            }))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<_>>()?;
    Ok((filings.len(), rows))
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU64, NonZeroUsize};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use httpmock::Method::POST;
    use httpmock::{Mock, MockServer};
    use jiff::Timestamp;
    use serde::Deserialize;
    use serde_json::json;

    use pipette_core::campaign::{CampaignOwner, NewCampaign};
    use pipette_core::config::{Config, DuckDbConfig, DuckDbPassword, StorageConfig};
    use pipette_core::plan::{PlanNotes, SettledOutcome, sample_manifest};
    use pipette_core::types::Mint;
    use pipette_plan_types::{
        AuditActor, BenchmarkType, CampaignState, NonEmptyTrimmedString, OrgId, PolicySettings,
        UserId,
    };
    use pipette_store::{AuditFilter, Stores};
    use pipette_store_sql::scratch::TestDb;

    use super::*;
    use crate::plan_ingestion::{JobRejectionError, RejectionError};
    use crate::submission::{Submission, parse_stored_submission};

    /// A case of `ui/test/policy-checks.json`: one job's rows, and the checks
    /// that flag it with spread's threshold at `percent`.
    #[derive(Deserialize)]
    struct Case {
        name: String,
        percent: f64,
        flagged: Vec<PolicyKind>,
        rows: Vec<Value>,
    }

    fn fixture() -> anyhow::Result<Vec<Case>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/test/policy-checks.json");
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// An evaluation reads its job sources once. Later calls use the cached
    /// sources even after the test deletes the stored source. The first use
    /// reads only the jobs it names.
    #[tokio::test]
    async fn the_job_sources_are_read_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(
            &Config {
                storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                ..Config::default()
            },
            pipette_store_testkit::no_queue(),
        )?;
        let job_id = JobId::try_new("job-01a1bced-6a57-72a5-9b39-8f2a41c52513")?;
        let unnamed = JobId::try_new("job-01a1bced-6a57-72a5-9b39-8f2b197c7306")?;
        let body = json!({"spec": {"benchmark": "prefill_throughput_512"}});
        stores.plans.put_job_source(&job_id, &body).await?;
        stores.plans.put_job_source(&unnamed, &body).await?;
        let sources = Sources::new(&*stores.plans, vec![job_id.clone(), unnamed.clone()]);
        let wanted = std::slice::from_ref(&job_id);

        let first = sources.bodies(wanted).await?;
        let read: Vec<&JobId> = first.keys().collect();
        std::fs::remove_file(
            dir.path()
                .join("job-sources")
                .join(format!("{job_id}.json")),
        )?;
        let second = sources.bodies(wanted).await?.get(&job_id).cloned();

        assert_eq!(read, vec![&job_id]);
        assert_eq!(second, Some(body));
        Ok(())
    }

    /// `ui/test/policy-checks.test.ts` runs the console's checks over the same
    /// rows. Here each is read as pipette-duckdb's are, its numbers as `f32`.
    #[test]
    fn the_checks_flag_the_fixture_as_the_console_does() -> anyhow::Result<()> {
        let cases = fixture()?;
        let group_id = CampaignGroupId::mint();
        cases.iter().try_for_each(|case| {
            let rows = case
                .rows
                .iter()
                .map(|row| {
                    let sent = serde_json::to_value(serde_json::from_value::<
                        pipette_plan_types::ResultRow,
                    >(row.clone())?)?;
                    let foreign: Vec<&String> = row
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(field, _)| field)
                        .filter(|field| sent.get(field.as_str()).is_none())
                        .collect();
                    assert!(
                        foreign.is_empty(),
                        "{}: not the API's: {foreign:?}",
                        case.name
                    );
                    let read: ResultRow = serde_json::from_value(row.clone())?;
                    // What the console reads is what the API sends for the `f32`.
                    assert_eq!(
                        (console(read.value), console(read.value_stddev)),
                        (
                            row.get("value").and_then(Value::as_f64),
                            row.get("value_stddev").and_then(Value::as_f64)
                        ),
                        "{}",
                        case.name
                    );
                    Ok(read)
                })
                .collect::<anyhow::Result<Vec<ResultRow>>>()?;
            let flagged: Vec<PolicyKind> = PolicyKind::iter()
                .filter(|kind| {
                    let policy = CampaignGroupPolicy {
                        settings: PolicySettings {
                            percent: Some(case.percent),
                            ..PolicySettings::default()
                        },
                        ..kind.defaults(group_id.clone())
                    };
                    flags(&policy, &rows)
                })
                .collect();
            assert_eq!(flagged, case.flagged, "{}", case.name);
            anyhow::Ok(())
        })
    }

    #[rstest::rstest]
    #[case::a_tenth(100.05)]
    #[case::a_stddev(0.165)]
    #[case::exact(1000.5)]
    #[case::largest(f32::MAX)]
    #[case::smallest(f32::MIN_POSITIVE)]
    #[case::nan(f32::NAN)]
    #[case::infinite(f32::INFINITY)]
    #[case::negative_infinite(f32::NEG_INFINITY)]
    fn a_value_reads_as_the_api_sends_it(#[case] value: f32) -> anyhow::Result<()> {
        let sent: Option<f64> = serde_json::from_str(&serde_json::to_string(&Some(value))?)?;
        assert_eq!(console(Some(value)), sent);
        Ok(())
    }

    #[rstest::rstest]
    #[case::runtime_error("client", Some("runtime_error"), true)]
    #[case::device_error("client", Some("device_error"), true)]
    #[case::device_not_ready("client", Some("device_not_ready"), true)]
    #[case::unspecified("client", Some("unspecified"), true)]
    #[case::out_of_memory("client", Some("out_of_memory"), true)]
    #[case::attempt_timeout("client", Some("attempt_timeout"), false)]
    #[case::harness_error("client", Some("harness_error"), true)]
    #[case::no_code("client", None, false)]
    #[case::marked("marked", Some("unspecified"), false)]
    #[case::smaller_run_failed("smaller_run_failed", Some("attempt_timeout"), false)]
    fn the_failed_check_flags_a_device_failure_a_retry_may_fix(
        #[case] source: &str,
        #[case] code: Option<&str>,
        #[case] flagged: bool,
    ) -> anyhow::Result<()> {
        let policy = PolicyKind::Failed.defaults(CampaignGroupId::mint());
        assert_eq!(flags(&policy, &[failure(source, code)?]), flagged);
        Ok(())
    }

    /// The failed check flags a device's failure by the codes its policy lists
    /// alone, so a default code it leaves out is not flagged.
    #[rstest::rstest]
    #[case::listed(Some("harness_error"), true)]
    #[case::device_not_ready(Some("device_not_ready"), true)]
    #[case::a_default_left_out(Some("runtime_error"), false)]
    #[case::no_code(None, false)]
    fn the_failed_check_flags_the_codes_its_policy_lists(
        #[case] code: Option<&str>,
        #[case] flagged: bool,
    ) -> anyhow::Result<()> {
        let policy = CampaignGroupPolicy {
            settings: PolicySettings {
                codes: Some(
                    [
                        (FailureCode::HarnessError, 2),
                        (FailureCode::DeviceNotReady, 1),
                    ]
                    .into(),
                ),
                ..PolicySettings::default()
            },
            ..PolicyKind::Failed.defaults(CampaignGroupId::mint())
        };
        assert_eq!(flags(&policy, &[failure("client", code)?]), flagged);
        Ok(())
    }

    /// A failed row of `job-1` that `source` reported with `code`.
    fn failure(source: &str, code: Option<&str>) -> anyhow::Result<ResultRow> {
        Ok(serde_json::from_value(json!({
            "job_id": "job-1", "benchmark_id": "decode_throughput_512_100",
            "benchmark_type": "decode_throughput", "metric": "decode", "value": null,
            "value_stddev": null, "unit": "tokens/sec", "failed": true,
            "failure_source": source, "failure_code": code,
            "parameters": {}, "observations": {}, "device": {},
        }))?)
    }

    /// Each check flags a run exactly when the value a re-run records is over
    /// its threshold, on the fixture's rows and on failures, with no short
    /// repetition rule and failed's default codes.
    #[test]
    fn a_check_flags_what_its_value_puts_over_its_threshold() -> anyhow::Result<()> {
        let group_id = CampaignGroupId::mint();
        let fixture = fixture()?
            .into_iter()
            .map(|case| {
                let rows = case
                    .rows
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<Result<Vec<ResultRow>, _>>()?;
                anyhow::Ok((case.percent, rows))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let failures = [
            ("client", Some("runtime_error")),
            ("client", Some("out_of_memory")),
            ("client", None),
            ("marked", Some("unspecified")),
        ]
        .into_iter()
        .map(|(source, code)| Ok((5.0, vec![failure(source, code)?])))
        .collect::<anyhow::Result<Vec<_>>>()?;
        fixture.iter().chain(&failures).for_each(|(percent, rows)| {
            PolicyKind::iter().for_each(|kind| {
                let defaults = kind.defaults(group_id.clone());
                let policy = CampaignGroupPolicy {
                    settings: PolicySettings {
                        percent: Some(*percent),
                        codes: defaults.settings.codes.clone(),
                        ..PolicySettings::default()
                    },
                    ..defaults
                };
                let over =
                    value(&policy, rows).is_some_and(|value| match (kind, threshold(&policy)) {
                        (PolicyKind::TtftOverE2e, Some(threshold)) => {
                            ((value - threshold) * 10.0).round() > 0.0
                        }
                        (_, threshold) => value > threshold.unwrap_or(0.0),
                    });
                assert_eq!(flags(&policy, rows), over, "{kind}: {rows:?}");
            });
        });
        Ok(())
    }

    /// A measured row of `metric` in `unit` at `value`, its stddev `spread`
    /// percent of it, with `parameters` and `observations`.
    fn timed(
        (metric, unit): (&str, &str),
        value: f32,
        spread: f32,
        parameters: Value,
        observations: Value,
    ) -> anyhow::Result<ResultRow> {
        Ok(serde_json::from_value(json!({
            "job_id": "job-1", "benchmark_id": "decode_throughput_512_100",
            "benchmark_type": "decode_throughput", "metric": metric, "value": value,
            "value_stddev": value * spread / 100.0, "unit": unit, "failed": false,
            "failure_source": null, "failure_code": null, "parameters": parameters,
            "observations": observations, "device": {},
        }))?)
    }

    const DECODE: (&str, &str) = ("decode_throughput", "tokens/sec");

    /// A rate's repetition takes its count over the rate, read where `score.rs`
    /// derives the rate from; a time's is its value.
    #[rstest::rstest]
    #[case::ttft(("ttft", "ms"), 200.0, json!({}), json!({}), Some(200.0))]
    #[case::prefill(("prefill_throughput", "tokens/sec"), 1280.0, json!({"prefill_tokens": 256}), json!({}), Some(200.0))]
    #[case::vl_prefill(("prefill_throughput", "tokens/sec"), 500.0, json!({}), json!({"vl_throughput_prefill_tokens": 400}), Some(800.0))]
    #[case::asr_prefill(("prefill_throughput", "tokens/sec"), 250.0, json!({}), json!({"asr_throughput_prefill_tokens": 100}), Some(400.0))]
    #[case::decode(DECODE, 125.0, json!({"prefill_tokens": 512, "decode_tokens": 100}), json!({}), Some(800.0))]
    #[case::decision(("decision_throughput", "requests/sec"), 8.0, json!({"batch_size": 4}), json!({}), Some(500.0))]
    #[case::end_to_end(("end_to_end_latency", "ms"), 4000.0, json!({}), json!({}), Some(4000.0))]
    #[case::no_count(DECODE, 125.0, json!({}), json!({}), None)]
    #[case::ratio(("accuracy", "ratio"), 0.5, json!({}), json!({}), None)]
    fn a_repetition_takes_its_rows_time(
        #[case] metric: (&str, &str),
        #[case] value: f32,
        #[case] parameters: Value,
        #[case] observations: Value,
        #[case] ms: Option<f64>,
    ) -> anyhow::Result<()> {
        let row = timed(metric, value, 1.0, parameters, observations)?;
        assert_eq!(repetition_ms(&row), ms);
        Ok(())
    }

    /// Under the short repetition rule, a row whose repetitions average under
    /// its time is weighed against its percent, and any other row against
    /// `percent`, a row whose time cannot be read included.
    #[rstest::rstest]
    #[case::short_under(125.0, 8.0, json!({"decode_tokens": 100}), true, false)]
    #[case::short_over(125.0, 12.0, json!({"decode_tokens": 100}), true, true)]
    #[case::long(40.0, 8.0, json!({"decode_tokens": 100}), true, true)]
    #[case::no_count(125.0, 8.0, json!({}), true, true)]
    #[case::no_rule(125.0, 8.0, json!({"decode_tokens": 100}), false, true)]
    fn spread_weighs_a_short_repetition_against_its_percent(
        #[case] value: f32,
        #[case] spread: f32,
        #[case] parameters: Value,
        #[case] rule: bool,
        #[case] flagged: bool,
    ) -> anyhow::Result<()> {
        let rows = [timed(DECODE, value, spread, parameters, json!({}))?];
        let policy = CampaignGroupPolicy {
            settings: PolicySettings {
                percent: Some(5.0),
                short_repetition_ms: rule.then_some(1000.0),
                short_repetition_percent: rule.then_some(10.0),
                codes: None,
            },
            ..PolicyKind::Spread.defaults(CampaignGroupId::mint())
        };
        assert_eq!(flags(&policy, &rows), flagged);
        Ok(())
    }

    /// A `kind` re-run of `job-1` as `job-2` in `campaign_x`'s head, filed now
    /// at the kind's default codes, waiting.
    fn a_rerun(kind: PolicyKind) -> anyhow::Result<PolicyRerun> {
        Ok(PolicyRerun {
            campaign_id: CampaignId::try_new("campaign_x")?,
            kind,
            rerun_job_id: job("job-2"),
            rerun_of: job("job-1"),
            plan_id: PlanId::try_new(HEAD)?,
            filed_at: Timestamp::now(),
            attempt: 1,
            benchmark_id: "decode_throughput_512_100".to_owned(),
            model_key: None,
            before: 1.0,
            threshold: None,
            short_repetition_ms: None,
            short_repetition_percent: None,
            codes: kind
                .defaults(CampaignGroupId::mint())
                .settings
                .codes
                .map(|codes| codes.into_keys().collect()),
            before_code: None,
            after: None,
            after_code: None,
            outcome: None,
            judged_at: None,
        })
    }

    /// A re-run is judged by the short repetition rule it was filed under,
    /// whatever the policy holds now.
    #[rstest::rstest]
    #[case::filed_with_the_rule(Some(1000.0), Some(10.0), RerunOutcome::Cleared)]
    #[case::filed_without(None, None, RerunOutcome::Flagged)]
    fn a_rerun_is_judged_by_its_filings_short_repetition_rule(
        #[case] short_repetition_ms: Option<f64>,
        #[case] short_repetition_percent: Option<f64>,
        #[case] outcome: RerunOutcome,
    ) -> anyhow::Result<()> {
        let waiting = PolicyRerun {
            before: 8.0,
            threshold: Some(5.0),
            short_repetition_ms,
            short_repetition_percent,
            ..a_rerun(PolicyKind::Spread)?
        };
        let rows = [timed(
            DECODE,
            125.0,
            8.0,
            json!({"decode_tokens": 100}),
            json!({}),
        )?];
        let judged = judged(&CampaignGroupId::mint(), &waiting, &rows, Timestamp::now());
        assert_eq!(judged.outcome, Some(outcome));
        Ok(())
    }

    /// A failed re-run is judged by the codes it was filed under, whatever the
    /// policy holds now: a re-run that fails again with one of them is still
    /// flagged, and one that fails with another code failed.
    #[rstest::rstest]
    #[case::filed_with_the_code(FailureCode::HarnessError, 1.0, RerunOutcome::Flagged)]
    #[case::filed_without(FailureCode::RuntimeError, 0.0, RerunOutcome::Failed)]
    fn a_failed_rerun_is_judged_by_its_filings_codes(
        #[case] filed: FailureCode,
        #[case] after: f64,
        #[case] outcome: RerunOutcome,
    ) -> anyhow::Result<()> {
        let waiting = PolicyRerun {
            codes: Some(vec![filed]),
            before_code: Some(filed.to_string()),
            ..a_rerun(PolicyKind::Failed)?
        };
        let rows = [failure("client", Some("harness_error"))?];
        let judged = judged(&CampaignGroupId::mint(), &waiting, &rows, Timestamp::now());
        assert_eq!(
            (judged.after, judged.after_code.as_deref(), judged.outcome),
            (Some(after), Some("harness_error"), Some(outcome))
        );
        Ok(())
    }

    /// A failed run gives the code of the row the check flags, else of any
    /// failed row; the other checks give none.
    #[test]
    fn a_failed_run_gives_its_failure_code() -> anyhow::Result<()> {
        let rows = [
            failure("client", Some("attempt_timeout"))?,
            failure("client", Some("device_error"))?,
        ];
        let code = |kind: PolicyKind, rows: &[ResultRow]| {
            failure_code(&kind.defaults(CampaignGroupId::mint()), rows)
        };
        assert_eq!(
            code(PolicyKind::Failed, &rows).as_deref(),
            Some("device_error")
        );
        assert_eq!(
            code(PolicyKind::Failed, &rows[..1]).as_deref(),
            Some("attempt_timeout")
        );
        assert_eq!(code(PolicyKind::Swap, &rows), None);
        Ok(())
    }

    /// A row of `job_id` at `benchmark_id`: failed from `failure` (source and
    /// code), else measured for `MODEL`.
    fn sized(
        job_id: &str,
        benchmark_id: &str,
        failure: Option<(&str, &str)>,
    ) -> anyhow::Result<ResultRow> {
        let (source, code) = failure.unzip();
        Ok(serde_json::from_value(json!({
            "job_id": job_id, "benchmark_id": benchmark_id,
            "benchmark_type": "decode_throughput", "metric": "decode",
            "value": failure.is_none().then_some(10.0), "value_stddev": null,
            "unit": "tokens/sec", "failed": failure.is_some(),
            "failure_source": source, "failure_code": code,
            "model_descriptor": failure.is_none().then_some(MODEL),
            "parameters": {}, "observations": {}, "device": {},
        }))?)
    }

    const MODEL: &str = r#"{"type":"mlx","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-350M-MLX-4bit"}"#;

    /// A failed job is flagged against the smallest clean job of its family
    /// that is no smaller in any size, whatever its failure; a failed job with
    /// only smaller or other families' clean jobs, or with no family, is not.
    #[test]
    fn a_failure_below_a_clean_larger_run_is_flagged() -> anyhow::Result<()> {
        let jobs = [
            (
                "oom-256",
                "f",
                vec![256, 100],
                Some(("client", "out_of_memory")),
            ),
            (
                "unrun-512",
                "f",
                vec![512, 100],
                Some(("smaller_run_failed", "attempt_timeout")),
            ),
            ("clean-1024", "f", vec![1024, 100], None),
            ("clean-4096", "f", vec![4096, 100], None),
            // Fewer decode tokens: not larger than the failures above.
            ("clean-2048-short", "f", vec![2048, 50], None),
            (
                "marked-4096",
                "g",
                vec![4096, 100],
                Some(("marked", "unspecified")),
            ),
            ("clean-256", "g", vec![256, 100], None),
            (
                "expired-128",
                "h",
                vec![128, 100],
                Some(("expired", "unspecified")),
            ),
            ("clean-other", "i", vec![1024, 100], None),
        ];
        let families: Families = jobs
            .iter()
            .map(|(id, key, sizes, _)| (job(id), ((*key).to_owned(), sizes.clone())))
            .collect();
        let rows: HashMap<JobId, Vec<ResultRow>> = jobs
            .iter()
            .map(|(id, _, sizes, failure)| {
                let benchmark = format!("decode_throughput_{}_{}", sizes[0], sizes[1]);
                Ok((job(id), vec![sized(id, &benchmark, *failure)?]))
            })
            .chain([Ok((
                job("no-family"),
                vec![sized(
                    "no-family",
                    "decode_throughput_64_100",
                    Some(("client", "device_error")),
                )?],
            ))])
            .collect::<anyhow::Result<_>>()?;
        let flagged = |job_id: &str, benchmark: &str, source, code| SmallerFailure {
            job_id: job(job_id),
            benchmark_id: benchmark.to_owned(),
            failure_source: Some(source),
            failure_code: Some(code),
            model_key: descriptor_key(MODEL),
            clean_job_id: job("clean-1024"),
            clean_benchmark_id: "decode_throughput_1024_100".to_owned(),
        };
        assert_eq!(
            smaller_failures(&families, &rows),
            [
                flagged(
                    "oom-256",
                    "decode_throughput_256_100",
                    FailureSource::Client,
                    FailureCode::OutOfMemory
                ),
                flagged(
                    "unrun-512",
                    "decode_throughput_512_100",
                    FailureSource::SmallerRunFailed,
                    FailureCode::AttemptTimeout
                ),
            ]
        );
        assert!(descriptor_key(MODEL).is_some());
        Ok(())
    }

    /// The plan each test's campaign runs.
    const HEAD: &str = "plan-head";

    /// The columns the mock pipette-duckdb answers.
    const COLUMNS: [&str; 11] = [
        "job_id",
        "benchmark_id",
        "benchmark_type",
        "metric",
        "value",
        "value_stddev",
        "unit",
        "failed",
        "failure_source",
        "observation_max_swap_bytes",
        "failure_code",
    ];

    fn job(name: &str) -> JobId {
        JobId::new_unchecked(name)
    }

    /// A job state the pass is done with on `name`, which `flagged` flag.
    fn decided_state(name: &str, flagged: &[PolicyKind]) -> anyhow::Result<JobPolicyState> {
        Ok(JobPolicyState {
            job_id: JobId::try_new(name)?,
            flagged: flagged.iter().copied().collect(),
            decided: true,
            missing_params: BTreeSet::new(),
            family: None,
            failed: false,
        })
    }

    /// The rows of a run that the checks of `kinds` flag, of swap and spread.
    fn run(job_id: &str, kinds: &[PolicyKind]) -> Vec<Value> {
        let swap = if kinds.contains(&PolicyKind::Swap) {
            4096
        } else {
            0
        };
        let stddev = if kinds.contains(&PolicyKind::Spread) {
            10.0
        } else {
            1.0
        };
        vec![json!([
            job_id,
            "decode_throughput_512_100",
            "decode_throughput",
            "decode",
            100.0,
            stddev,
            "tokens/sec",
            false,
            null,
            swap,
            null
        ])]
    }

    /// The rows of a run whose result's standard deviation is `percent` of its
    /// value.
    fn spread_of(job_id: &str, percent: f64) -> Vec<Value> {
        vec![json!([
            job_id,
            "decode_throughput_512_100",
            "decode_throughput",
            "decode",
            100.0,
            percent,
            "tokens/sec",
            false,
            null,
            0,
            null
        ])]
    }

    /// The rows of a run whose time to first token is over its end-to-end
    /// latency.
    fn ttft_over(job_id: &str) -> Vec<Value> {
        [("ttft", 200.0), ("end_to_end_latency", 100.0)]
            .into_iter()
            .map(|(metric, ms)| {
                json!([
                    job_id,
                    "end_to_end_latency_512_100",
                    "end_to_end_latency",
                    metric,
                    ms,
                    1.0,
                    "ms",
                    false,
                    null,
                    0,
                    null
                ])
            })
            .collect()
    }

    /// The rows of a run failed with `code`, as `source` reports it: `client`
    /// for its device, `marked` for a mark.
    fn failed_run(job_id: &str, source: &str, code: &str) -> Vec<Value> {
        vec![json!([
            job_id,
            "decode_throughput_512_100",
            "decode_throughput",
            "decode",
            null,
            null,
            "tokens/sec",
            true,
            source,
            null,
            code
        ])]
    }

    /// The rows of a mark: a failure that no device ran.
    fn mark(job_id: &str) -> Vec<Value> {
        failed_run(job_id, "marked", "unspecified")
    }

    /// The columns of [`modeled`].
    const MODEL_COLUMNS: [&str; 9] = [
        "job_id",
        "benchmark_id",
        "benchmark_type",
        "metric",
        "unit",
        "model_descriptor",
        "model_vault_descriptor",
        "model_params_total_millions",
        "model_params_active_millions",
    ];

    /// A row of `job_id` that records the descriptors `model` and `vault` and
    /// the stored counts `total` and `active`.
    fn modeled(
        job_id: &str,
        model: Option<&str>,
        vault: Option<&str>,
        total: Option<i32>,
        active: Option<i32>,
    ) -> Value {
        json!([
            job_id,
            "decode_throughput_512_100",
            "decode_throughput",
            "decode",
            "tokens/sec",
            model,
            vault,
            total,
            active
        ])
    }

    /// The document [`World::filed`] files: one job per model, five on macOS.
    const DOCUMENT: &str = r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models = [
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-350M-MLX-4bit" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-1.2B-MLX-4bit" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-2.6B-MLX-4bit" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-8B-A1B-MLX-4bit" },
]
runtimes = [
  { type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } },
]
"#;

    const MAX_JOBS: NonZeroUsize = match NonZeroUsize::new(5000) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    };
    const CREATING_MAX_AGE: Duration = Duration::from_secs(21_600);

    /// A plan index backend probe that passes.
    fn probed() -> std::future::Ready<anyhow::Result<()>> {
        std::future::ready(Ok(()))
    }

    /// The settings of a test ingest: the limits above, no model check and no
    /// import.
    fn settings() -> IngestSettings<'static> {
        IngestSettings {
            descriptor_config: crate::plan_ingestion::no_offer_rules(),
            models: crate::plan_ingestion::ModelCheck::none(),
            max_jobs: MAX_JOBS,
            creating_max_age: CREATING_MAX_AGE,
            import: None,
        }
    }

    /// Once the filed plan is first written, its campaign's head is stopped:
    /// a cancel that lands while the ingest runs.
    const STOP_THE_HEAD: &str = "CREATE FUNCTION stop_the_head() RETURNS trigger LANGUAGE plpgsql
            AS $$ BEGIN
                INSERT INTO cancelled_plans (plan_id)
                SELECT body::jsonb ->> 'current_plan_id' FROM campaigns
                WHERE campaign_id = NEW.campaign_id
                ON CONFLICT (plan_id) DO NOTHING;
                RETURN NULL;
            END $$;
        CREATE TRIGGER stop_the_head AFTER INSERT ON plans
            FOR EACH ROW EXECUTE FUNCTION stop_the_head();";

    /// Once the filed plan is first written, its campaign moves onto the plan
    /// it runs neither as its head nor as the filed one, which a person staged:
    /// a revision that lands while the ingest runs.
    const A_PERSON_REVISES: &str = "CREATE FUNCTION a_person_revises() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN
                UPDATE campaigns c
                SET body = jsonb_set(c.body::jsonb, '{current_plan_id}', to_jsonb(p.plan_id))::text
                FROM plans p
                WHERE c.campaign_id = NEW.campaign_id AND p.campaign_id = NEW.campaign_id
                    AND p.plan_id <> NEW.plan_id
                    AND p.plan_id <> c.body::jsonb ->> 'current_plan_id';
                RETURN NULL;
            END $$;
        CREATE TRIGGER a_person_revises AFTER INSERT ON plans
            FOR EACH ROW EXECUTE FUNCTION a_person_revises();";

    /// Every cancel marker write of a plan no campaign runs fails: a filing's,
    /// while [`STOP_THE_HEAD`] still stops the head.
    const REFUSE_CANCEL: &str = "CREATE FUNCTION refuse_cancel() RETURNS trigger LANGUAGE plpgsql
            AS $$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM campaigns
                               WHERE body::jsonb ->> 'current_plan_id' = NEW.plan_id) THEN
                    RAISE EXCEPTION 'cancel refused';
                END IF;
                RETURN NEW;
            END $$;
        CREATE TRIGGER refuse_cancel BEFORE INSERT ON cancelled_plans
            FOR EACH ROW EXECUTE FUNCTION refuse_cancel();";

    /// Every campaign write fails.
    const REFUSE_HEAD_WRITE: &str = "CREATE FUNCTION refuse_head_write() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'head write refused'; END $$;
        CREATE TRIGGER refuse_head_write BEFORE UPDATE ON campaigns
            FOR EACH ROW EXECUTE FUNCTION refuse_head_write();";

    /// Once the filed plan is final, its campaign's head no longer decodes, so
    /// the check after the ingest cannot read it.
    const UNREADABLE_HEAD: &str = "CREATE FUNCTION unreadable_head() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN
                UPDATE plans SET body = 'not a plan'
                WHERE plan_id = (SELECT body::jsonb ->> 'current_plan_id' FROM campaigns
                                 WHERE campaign_id = NEW.campaign_id);
                RETURN NULL;
            END $$;
        CREATE TRIGGER unreadable_head AFTER UPDATE ON plans
            FOR EACH ROW WHEN (OLD.status = 'creating' AND NEW.status <> 'creating')
            EXECUTE FUNCTION unreadable_head();";

    /// Once the filed plan is final, its campaign no longer decodes, so the
    /// advance cannot read it.
    const UNREADABLE_CAMPAIGN: &str = "CREATE FUNCTION unreadable_campaign() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN
                UPDATE campaigns SET body = 'not a campaign' WHERE campaign_id = NEW.campaign_id;
                RETURN NULL;
            END $$;
        CREATE TRIGGER unreadable_campaign AFTER UPDATE ON plans
            FOR EACH ROW WHEN (OLD.status = 'creating' AND NEW.status <> 'creating')
            EXECUTE FUNCTION unreadable_campaign();";

    /// Every write of a member's kept state fails.
    const REFUSE_STATE_WRITE: &str = "CREATE FUNCTION refuse_state_write() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'state write refused'; END $$;
        CREATE TRIGGER refuse_state_write BEFORE UPDATE ON campaign_group_members
            FOR EACH ROW EXECUTE FUNCTION refuse_state_write();";

    /// Every write of a recorded re-run's outcome fails.
    const REFUSE_OUTCOME_WRITE: &str = "CREATE FUNCTION refuse_outcome_write() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'outcome write refused'; END $$;
        CREATE TRIGGER refuse_outcome_write BEFORE UPDATE ON campaign_group_rerun_results
            FOR EACH ROW EXECUTE FUNCTION refuse_outcome_write();";

    /// The stored documents set a deadline that has passed.
    const PASSED_DEADLINE: &str = "UPDATE plan_sources
        SET source = 'expires_at = \"2020-01-01T00:00:00Z\"' || chr(10) || source";

    /// The stored documents no longer parse.
    const UNPARSABLE: &str = "UPDATE plan_sources SET source = source || chr(10) || '[[variants'";

    /// A member campaign of a group, its plans on a new database, and a mock
    /// pipette-duckdb that answers only the queries a test sets up.
    struct World {
        db: TestDb,
        stores: Stores,
        duckdb: MockServer,
        group: CampaignGroup,
        campaign: Campaign,
        /// The jobs of the campaign's first plan, in its order; none where a
        /// test puts the plans by hand.
        jobs: Vec<JobId>,
        dir: tempfile::TempDir,
    }

    impl World {
        /// `None` skips the test without `TEST_DATABASE_URL`.
        async fn new() -> anyhow::Result<Option<Self>> {
            let Some(db) = TestDb::new().await? else {
                return Ok(None);
            };
            let dir = tempfile::tempdir()?;
            let stores = db.stores(pipette_store_fs::build_local_fs_stores(
                &Config {
                    storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                    auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
                    ..Config::default()
                },
                pipette_store_testkit::no_queue(),
            )?);
            let owner = CampaignOwner::User {
                user_id: UserId::try_new("user_nightly")?,
            };
            let group = CampaignGroup::new(
                NonEmptyTrimmedString::try_new("aa-perf")?,
                owner.clone(),
                Timestamp::now(),
            );
            let campaign = NewCampaign::new(NonEmptyTrimmedString::try_new("S26 CPU")?, owner)
                .around(PlanId::try_new(HEAD)?, Timestamp::now());
            Ok(Some(Self {
                db,
                stores,
                duckdb: MockServer::start_async().await,
                group,
                campaign,
                jobs: Vec::new(),
                dir,
            }))
        }

        /// The group stored, with the campaign filed from [`DOCUMENT`] as its
        /// member: five jobs, each queued. `None` skips the test without
        /// `TEST_DATABASE_URL`.
        async fn filed() -> anyhow::Result<Option<Self>> {
            let Some(world) = Self::new().await? else {
                return Ok(None);
            };
            world.groups()?.create_group(&world.group).await?;
            let benchmarks = world.dir.path().join("benchmarks");
            std::fs::create_dir_all(&benchmarks)?;
            std::fs::write(
                benchmarks.join("decode_throughput_512_100.toml"),
                "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 512\n\
                 parameter_decode_tokens = 100",
            )?;
            let (campaign, jobs) = world.join("S26 CPU", DOCUMENT).await?;
            Ok(Some(Self {
                campaign,
                jobs,
                ..world
            }))
        }

        /// A campaign named `name` filed from `document` as a member of the
        /// group, and its jobs in plan order.
        async fn join(&self, name: &str, document: &str) -> anyhow::Result<(Campaign, Vec<JobId>)> {
            let path = self.dir.path().join(format!("{name}.toml"));
            std::fs::write(&path, document)?;
            let new = NewCampaign::new(
                NonEmptyTrimmedString::try_new(name)?,
                self.group.owner.clone(),
            );
            let (campaign, report) =
                campaign_admin::create(&self.stores, settings(), new, &path, None).await?;
            self.groups()?
                .add_members(
                    &self.group.group_id,
                    std::slice::from_ref(&campaign.campaign_id),
                )
                .await?;
            let jobs = report
                .job_ids
                .into_iter()
                .map(|(_, job_id)| job_id)
                .collect();
            Ok((campaign, jobs))
        }

        /// What [`apply`] reads: the mock pipette-duckdb.
        fn config(&self) -> Config {
            Config {
                duckdb: Some(DuckDbConfig {
                    url: self.duckdb.base_url(),
                    user: "mgmt".to_string(),
                    password: DuckDbPassword::new("secret"),
                    database: "internal".to_string(),
                    timeout_secs: NonZeroU64::MIN,
                }),
                ..Config::default()
            }
        }

        fn groups(&self) -> anyhow::Result<&dyn CampaignGroupStore> {
            self.stores
                .campaign_groups
                .as_deref()
                .context("a [database] keeps campaign groups")
        }

        /// Store `policies`, none of whose kinds was written yet.
        async fn turn_on(&self, policies: &[CampaignGroupPolicy]) -> anyhow::Result<()> {
            let groups = self.groups()?;
            futures::stream::iter(policies)
                .map(Ok)
                .try_for_each(|policy| async move {
                    let defaults = policy.kind.defaults(policy.group_id.clone());
                    let written = groups.replace_policy(&defaults, policy).await?;
                    anyhow::ensure!(written, "the {} policy was written already", policy.kind);
                    Ok(())
                })
                .await
        }

        /// The campaign as stored now.
        async fn current(&self) -> anyhow::Result<Campaign> {
            Ok(
                campaign_admin::require(&*self.stores.campaigns, &self.campaign.campaign_id)
                    .await?,
            )
        }

        async fn manifest(&self, plan_id: &PlanId) -> anyhow::Result<PlanManifest> {
            self.stores
                .plans
                .get_plan(plan_id)
                .await?
                .with_context(|| format!("plan {plan_id}"))
        }

        /// What the member row keeps; `None` before the first write.
        async fn kept(&self) -> anyhow::Result<Option<GroupPolicyState>> {
            self.kept_of(&self.campaign.campaign_id).await
        }

        /// The campaign's job states, in job id order.
        async fn job_states(&self) -> anyhow::Result<Vec<JobPolicyState>> {
            self.groups()?
                .job_states(&self.group.group_id, &self.campaign.campaign_id)
                .await
        }

        /// The group and the campaign stored, the campaign its member, so what
        /// a round reads can be kept.
        async fn enroll(&self) -> anyhow::Result<()> {
            let groups = self.groups()?;
            groups.create_group(&self.group).await?;
            self.stores.campaigns.put_campaign(&self.campaign).await?;
            groups
                .add_members(
                    &self.group.group_id,
                    std::slice::from_ref(&self.campaign.campaign_id),
                )
                .await
        }

        /// The jobs of the campaign's job states the pass is done with.
        async fn decided(&self) -> anyhow::Result<BTreeSet<JobId>> {
            Ok(self
                .job_states()
                .await?
                .into_iter()
                .filter(|job_state| job_state.decided)
                .map(|job_state| job_state.job_id)
                .collect())
        }

        /// Keep `state` and `job_states` for the campaign, as a round keeps what
        /// it read.
        async fn keep(
            &self,
            state: &GroupPolicyState,
            job_states: &[JobPolicyState],
        ) -> anyhow::Result<()> {
            let changes = job_state_changes(&self.job_states().await?, job_states);
            let kept = self
                .groups()?
                .set_policy_state(
                    &self.group.group_id,
                    &self.campaign.campaign_id,
                    state,
                    &changes,
                )
                .await?;
            anyhow::ensure!(kept, "the campaign is a member");
            Ok(())
        }

        /// The campaign's recorded re-runs, newest filing first.
        async fn reruns(&self) -> anyhow::Result<Vec<PolicyRerun>> {
            let campaign = std::slice::from_ref(&self.campaign.campaign_id);
            let (_, rows) = self
                .groups()?
                .rerun_results(&self.group.group_id, campaign, 0, None)
                .await?;
            Ok(rows)
        }

        /// What the row of the member `campaign_id` keeps.
        async fn kept_of(
            &self,
            campaign_id: &CampaignId,
        ) -> anyhow::Result<Option<GroupPolicyState>> {
            Ok(self.member(campaign_id).await?.policy_state?)
        }

        /// The member row of `campaign_id`, as a round reads it.
        async fn member(&self, campaign_id: &CampaignId) -> anyhow::Result<GroupMember> {
            self.groups()?
                .policy_states(&self.group.group_id)
                .await?
                .into_iter()
                .find(|member| member.campaign_id == *campaign_id)
                .context("the campaign is a member")
        }

        /// Record each of `jobs` finished, out of the queue.
        async fn finish(&self, jobs: &[&JobId]) -> anyhow::Result<()> {
            futures::stream::iter(jobs)
                .map(Ok)
                .try_for_each(|job_id| {
                    pipette_store_testkit::finish_job(
                        &*self.stores.submissions,
                        &*self.stores.todo,
                        job_id,
                    )
                })
                .await
        }

        /// Record `job_id` answered with the stored submission `body`, out of
        /// the queue.
        async fn submit(&self, job_id: &JobId, body: &Value) -> anyhow::Result<()> {
            self.stores
                .submissions
                .write_processed(job_id, body)
                .await?;
            self.stores.todo.delete_avail_by_job(job_id).await
        }

        /// Evaluate the campaign with no saved policy state. Read the bodies
        /// of answered jobs without rows. Use the benchmark from [`DOCUMENT`]
        /// and the supplied `waiting` re-runs.
        async fn round_with_bodies(
            &self,
            stored: &[CampaignGroupPolicy],
            waiting: &[PolicyRerun],
        ) -> anyhow::Result<Evaluation> {
            let duckdb = self.config().duckdb.context("the mock pipette-duckdb")?;
            let results = ResultsDb::new(reqwest::Client::new(), duckdb)?;
            let id = BenchmarkId::try_new("decode_throughput_512_100")?;
            let benchmark = Benchmark::from_toml(
                id.as_str(),
                "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 512\n\
                 parameter_decode_tokens = 100",
            )?;
            let catalog = HashMap::from([(id, benchmark)]);
            let state = GroupPolicyState::default();
            let pass = Pass {
                state: &state,
                todo: &*self.stores.todo,
                submissions: &*self.stores.submissions,
                preview: false,
                table: None,
                catalog: None,
                waiting,
                job_states: &[],
                bodies: Some(BodyReads {
                    catalog: &catalog,
                    stores: &self.stores,
                }),
            };
            evaluate(
                &*self.stores.plans,
                Some(&results),
                &self.group,
                &self.campaign,
                stored,
                pass,
            )
            .await
        }

        /// Lease the queued `job_id` to a registered device: maintenance
        /// recycles a lease whose device the roster does not know.
        async fn lease(&self, job_id: &JobId) -> anyhow::Result<()> {
            let expires_at = self
                .stores
                .todo
                .avail_for_jobs(std::slice::from_ref(job_id))
                .await?
                .into_iter()
                .next()
                .context("the job is queued")?
                .expires_at;
            let holder = pipette_core::client::Client {
                client_id: pipette_plan_types::ClientId::try_new("ev1_a")?,
                public_key: pipette_plan_types::PublicKeyHex::try_new(
                    "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                )?,
                organization: NonEmptyTrimmedString::try_new("lab")?,
                client_details: NonEmptyTrimmedString::try_new("mac mini")?,
                contact_email: pipette_plan_types::ContactEmail::try_new("lab@example.test")?,
                status: pipette_core::client::ClientStatus::Approved,
                registered_at: Timestamp::now(),
                device_profile: Default::default(),
                capabilities: Default::default(),
            };
            self.stores.clients.put_client(&holder).await?;
            let claimed = self
                .stores
                .todo
                .claim_job(
                    job_id,
                    expires_at,
                    &holder.client_id,
                    Timestamp::now() + jiff::SignedDuration::from_hours(1),
                )
                .await?;
            anyhow::ensure!(
                matches!(claimed, pipette_store::ClaimResult::Claimed(_)),
                "job {job_id} was not claimed"
            );
            Ok(())
        }

        /// A queue-maintenance run, whose pass 7 records the answered jobs.
        async fn maintain(&self) -> anyhow::Result<()> {
            let catalog = crate::org_catalog::load_server_catalog(&*self.stores.catalog).await?;
            crate::queue_maintenance::run(
                &*self.stores.todo,
                &*self.stores.clients,
                &*self.stores.tags,
                &*self.stores.submissions,
                &*self.stores.plans,
                &catalog,
                crate::queue_maintenance::MaintenanceSettings {
                    tmp_max: Duration::from_secs(86_400),
                    plan_creating_max: CREATING_MAX_AGE,
                    lost_lease_limit: 3,
                },
            )
            .await
        }

        /// The campaign's plans, of every status.
        async fn plan_count(&self) -> anyhow::Result<usize> {
            Ok(self
                .stores
                .plans
                .get_campaign_plans(&self.campaign.campaign_id)
                .await?
                .context("SQL reads a campaign's plans at once")?
                .len())
        }

        /// A person's re-run of `job_id` in `campaign`, filed and not moved
        /// onto yet.
        async fn stage_rerun(&self, campaign: &Campaign, job_id: &JobId) -> anyhow::Result<PlanId> {
            let plans = &*self.stores.plans;
            let revision = Head::read(plans, campaign.clone(), None)
                .await?
                .rerun(
                    plans,
                    &*self.stores.submissions,
                    BTreeSet::from([job_id.clone()]),
                    BTreeMap::new(),
                )
                .await?;
            let catalog = crate::org_catalog::load_server_catalog(&*self.stores.catalog).await?;
            let ctx = IngestContext::with_settings(&self.stores, &catalog, settings());
            let membership = revision.membership(plans).await?;
            let change = Some(revision.change());
            let input = PlanInput::Text(revision.document());
            Ok(plan_cli::ingest(&ctx, input, None, change, membership)
                .await?
                .plan_id)
        }

        /// [`file()`] what `evaluation` found in `campaign`.
        async fn file(
            &self,
            campaign: &Campaign,
            stored: &[CampaignGroupPolicy],
            evaluation: &Evaluation,
        ) -> anyhow::Result<Filing> {
            let groups = self.groups()?;
            Ok(super::file(
                &self.stores,
                groups,
                settings(),
                &self.group,
                campaign,
                stored,
                evaluation,
            )
            .await)
        }

        /// `policy` on, stored, and the first job of the campaign answered with
        /// a swap, as the rows the next round reads.
        async fn swapped(
            &self,
            policy: CampaignGroupPolicy,
        ) -> anyhow::Result<[CampaignGroupPolicy; 1]> {
            let stored = [policy];
            self.turn_on(&stored).await?;
            let swapped = self.jobs.first().context("a job")?;
            self.finish(&[swapped]).await?;
            self.maintain().await?;
            self.answer(&[(swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap]))])
                .await;
            Ok(stored)
        }

        /// [`Self::swapped`] with the swap policy at 3 re-runs.
        async fn one_swapped(&self) -> anyhow::Result<[CampaignGroupPolicy; 1]> {
            self.swapped(self.on(PolicyKind::Swap, 3)).await
        }

        /// One swapped job, read by a round that kept nothing.
        async fn evaluated(
            &self,
        ) -> anyhow::Result<([CampaignGroupPolicy; 1], Campaign, Evaluation)> {
            let stored = self.one_swapped().await?;
            let campaign = self.current().await?;
            let evaluation = self
                .round(&campaign, &stored, Some(&GroupPolicyState::default()))
                .await?;
            Ok((stored, campaign, evaluation))
        }

        /// The member taken as a round of the policy pass takes it, and what
        /// that counted.
        async fn pass(
            &self,
            stored: &[CampaignGroupPolicy],
        ) -> anyhow::Result<(Evaluation, Option<Filing>, PassCounts)> {
            let (config, groups) = (self.config(), self.groups()?);
            let duckdb = config.duckdb.clone().context("the mock pipette-duckdb")?;
            let results = ResultsDb::new(reqwest::Client::new(), duckdb)?;
            let member = self.member(&self.campaign.campaign_id).await?;
            let round = Round {
                config: &config,
                stores: &self.stores,
                groups,
                table: OnceCell::new(),
                catalog: OnceCell::new(),
                probed: OnceCell::new(),
                validate_index: &probed,
            };
            let mut counts = PassCounts::default();
            let (evaluation, filing) = round
                .take(Some(&results), &self.group, stored, &member, &mut counts)
                .await
                .context("the member was skipped")?;
            Ok((evaluation, filing, counts))
        }

        /// The re-run a round files of `job` once it finished with `rows`.
        async fn reran(
            &self,
            stored: &[CampaignGroupPolicy],
            job: &JobId,
            rows: Vec<Value>,
        ) -> anyhow::Result<JobId> {
            self.finish(&[job]).await?;
            self.maintain().await?;
            self.answer(&[(job.as_str(), rows)]).await;
            self.pass(stored).await?;
            self.reruns()
                .await?
                .into_iter()
                .find(|rerun| rerun.rerun_of == *job)
                .map(|rerun| rerun.rerun_job_id)
                .context("the job's re-run")
        }

        /// The rows [`backfill`] records once every recorded row is gone, while
        /// pipette-duckdb answers only `runs`.
        async fn backfilled(
            &self,
            runs: &[(&str, Vec<Value>)],
        ) -> anyhow::Result<Vec<PolicyRerun>> {
            self.db
                .execute("DELETE FROM campaign_group_rerun_results")
                .await?;
            self.duckdb.reset_async().await;
            self.answer(runs).await;
            let duckdb = self.config().duckdb.context("the mock pipette-duckdb")?;
            let results = ResultsDb::new(reqwest::Client::new(), duckdb)?;
            backfill(
                &self.stores,
                &results,
                std::slice::from_ref(&self.group.group_id),
            )
            .await?;
            self.reruns().await
        }

        /// Each audit event's outcome and targets, newest first.
        async fn audited(&self) -> anyhow::Result<Vec<(AuditOutcome, BTreeSet<AuditTarget>)>> {
            let log = self.stores.audit.as_deref().context("an audit log")?;
            let events = log
                .list_events(
                    &AuditFilter::default(),
                    None,
                    NonZeroUsize::MIN.saturating_add(9),
                )
                .await?;
            Ok(events
                .items
                .into_iter()
                .map(|event| (event.outcome, event.targets))
                .collect())
        }

        /// The targets a filing's audit event names.
        fn targets(&self, plans: &[&PlanId]) -> BTreeSet<AuditTarget> {
            [
                AuditTarget::Campaign {
                    campaign_id: self.campaign.campaign_id.clone(),
                },
                AuditTarget::CampaignGroup {
                    group_id: self.group.group_id.clone(),
                },
                AuditTarget::from(self.group.owner.clone()),
            ]
            .into_iter()
            .chain(plans.iter().map(|plan_id| AuditTarget::Plan {
                plan_id: (*plan_id).clone(),
            }))
            .collect()
        }

        /// An active plan of the campaign holding `jobs`, each answered, with
        /// the job it runs again if any.
        fn plan(
            &self,
            plan_id: &str,
            jobs: &[(&str, Option<&str>)],
        ) -> anyhow::Result<PlanManifest> {
            Ok(PlanManifest {
                campaign_id: Some(self.campaign.campaign_id.clone()),
                job_ids: jobs.iter().map(|(job_id, _)| job(job_id)).collect(),
                content_keys: jobs
                    .iter()
                    .map(|(job_id, _)| (job(job_id), "c".repeat(64)))
                    .collect(),
                rerun_of: jobs
                    .iter()
                    .filter_map(|(job_id, earlier)| Some((job(job_id), job((*earlier)?))))
                    .collect(),
                settled_jobs: jobs
                    .iter()
                    .map(|(job_id, _)| (job(job_id), SettledOutcome::Finished))
                    .collect(),
                expired_jobs: BTreeMap::new(),
                warnings: Vec::new(),
                ..sample_manifest(PlanId::try_new(plan_id)?, PlanStatus::Active, false)?
            })
        }

        /// File `plans`, each linked from its jobs.
        async fn put(&self, plans: &[PlanManifest]) -> anyhow::Result<()> {
            let store = &*self.stores.plans;
            futures::future::try_join_all(plans.iter().map(|plan| async move {
                store.put_plan(plan).await?;
                futures::future::try_join_all(
                    plan.job_ids
                        .iter()
                        .map(|job_id| store.put_job_plan_link(job_id, &plan.plan_id)),
                )
                .await?;
                anyhow::Ok(())
            }))
            .await?;
            Ok(())
        }

        /// Store each job's source, naming its benchmark.
        async fn source(&self, jobs: &[(&str, &str)]) -> anyhow::Result<()> {
            let plans = &*self.stores.plans;
            futures::future::try_join_all(jobs.iter().map(|(job_id, benchmark)| async move {
                let body = json!({ "spec": { "benchmark": benchmark } });
                plans.put_job_source(&job(job_id), &body).await
            }))
            .await?;
            Ok(())
        }

        /// Answer the query that names every job of `runs` with their rows.
        async fn answer(&self, runs: &[(&str, Vec<Value>)]) -> Mock<'_> {
            self.answer_in(&COLUMNS, runs).await
        }

        /// [`Self::answer`] with rows of `columns`.
        async fn answer_in(&self, columns: &[&str], runs: &[(&str, Vec<Value>)]) -> Mock<'_> {
            let rows: Vec<Value> = runs.iter().flat_map(|(_, rows)| rows.clone()).collect();
            let named: Vec<String> = runs
                .iter()
                .map(|(job_id, _)| format!("'{job_id}'"))
                .collect();
            self.duckdb
                .mock_async(|when, then| {
                    named
                        .into_iter()
                        .fold(when.method(POST).path("/api/query"), |when, named| {
                            when.body_includes(named)
                        });
                    then.status(200)
                        .json_body(json!({"columns": columns, "rows": rows}));
                })
                .await
        }

        /// The group's `kind` policy, on, with `max_reruns`; spread's at its
        /// defaults (5%, or 10% for repetitions under 1,000 ms).
        fn on(&self, kind: PolicyKind, max_reruns: u32) -> CampaignGroupPolicy {
            CampaignGroupPolicy {
                enabled: true,
                max_reruns,
                ..kind.defaults(self.group.group_id.clone())
            }
        }

        /// `campaign` as the preview reads it.
        async fn read(
            &self,
            campaign: &Campaign,
            stored: &[CampaignGroupPolicy],
        ) -> anyhow::Result<Evaluation> {
            self.round(campaign, stored, None).await
        }

        /// `campaign` as a round of the policy pass reads it, which kept
        /// `state` of it, or as the preview reads a member with none.
        async fn round(
            &self,
            campaign: &Campaign,
            stored: &[CampaignGroupPolicy],
            state: Option<&GroupPolicyState>,
        ) -> anyhow::Result<Evaluation> {
            self.round_with(campaign, stored, state, None).await
        }

        /// [`Self::round`] with `table` for the model params check.
        async fn round_with(
            &self,
            campaign: &Campaign,
            stored: &[CampaignGroupPolicy],
            state: Option<&GroupPolicyState>,
            table: Option<&ModelParamTable>,
        ) -> anyhow::Result<Evaluation> {
            let duckdb = self.config().duckdb.context("the mock pipette-duckdb")?;
            let results = ResultsDb::new(reqwest::Client::new(), duckdb)?;
            let none = GroupPolicyState::default();
            let job_states = match state {
                Some(_) => {
                    self.groups()?
                        .job_states(&self.group.group_id, &campaign.campaign_id)
                        .await?
                }
                None => Vec::new(),
            };
            let pass = Pass {
                state: state.unwrap_or(&none),
                todo: &*self.stores.todo,
                submissions: &*self.stores.submissions,
                preview: state.is_none(),
                table,
                catalog: None,
                waiting: &[],
                job_states: &job_states,
                bodies: None,
            };
            evaluate(
                &*self.stores.plans,
                Some(&results),
                &self.group,
                campaign,
                stored,
                pass,
            )
            .await
        }

        /// What the policies that are on would do with each job: `re-run`, the
        /// end of a mark's reason, after `spread: ` for a spread mark, or why
        /// the job is skipped.
        fn outcomes(&self, evaluation: &Evaluation) -> Vec<String> {
            let lead = |kind: PolicyKind| {
                format!(
                    "The {kind} policy of campaign group {} marked this job failed: ",
                    self.group.group_id
                )
            };
            let (swap, spread) = (lead(PolicyKind::Swap), lead(PolicyKind::Spread));
            let mut outcomes: Vec<String> = evaluation
                .rerun_job_ids
                .iter()
                .map(|job_id| format!("{job_id}: re-run"))
                .chain(evaluation.marked.iter().map(|(job_id, reason)| {
                    match (reason.strip_prefix(&swap), reason.strip_prefix(&spread)) {
                        (Some(why), _) => format!("{job_id}: {why}"),
                        (None, Some(why)) => format!("{job_id}: spread: {why}"),
                        (None, None) => format!("{job_id}: {reason}"),
                    }
                }))
                .chain(
                    evaluation
                        .skipped
                        .iter()
                        .map(|(job_id, reason)| format!("{job_id}: {reason:?}")),
                )
                .collect();
            outcomes.sort();
            outcomes
        }
    }

    /// `kind`'s counts: flagged, re-run, marked and kept.
    fn counts(evaluation: &Evaluation, kind: PolicyKind) -> Option<(usize, usize, usize, usize)> {
        evaluation
            .checks
            .iter()
            .find(|check| check.kind == kind)
            .map(|check| (check.flagged, check.rerun, check.marked, check.kept))
    }

    /// The jobs without rows wait and are counted; a pending job is not read.
    #[rstest::rstest]
    #[case::active(PlanStatus::Active)]
    #[case::pending_clients(PlanStatus::PendingClients)]
    #[case::complete(PlanStatus::Complete)]
    #[tokio::test]
    async fn a_plan_is_read_for_its_answered_jobs(
        #[case] status: PlanStatus,
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let head = world.plan(
            HEAD,
            &[
                ("job-1", None),
                ("job-2", None),
                ("job-3", None),
                ("job-4", None),
                ("job-5", None),
            ],
        )?;
        let head = PlanManifest {
            status,
            settled_jobs: head
                .settled_jobs
                .into_iter()
                .filter(|(job_id, _)| *job_id != job("job-3"))
                .collect(),
            content_keys: head
                .content_keys
                .into_iter()
                .filter(|(job_id, _)| *job_id != job("job-5"))
                .collect(),
            ..head
        };
        world.put(std::slice::from_ref(&head)).await?;
        let pending = world
            .duckdb
            .mock_async(|when, then| {
                when.method(POST).body_includes("'job-3'");
                then.status(500);
            })
            .await;
        let swap = [PolicyKind::Swap];
        world
            .answer(&[
                ("job-1", run("job-1", &swap)),
                ("job-2", Vec::new()),
                ("job-4", run("job-4", &[])),
                ("job-5", run("job-5", &swap)),
            ])
            .await;
        let evaluation = world
            .read(&world.campaign, &[world.on(PolicyKind::Swap, 3)])
            .await?;
        pending.assert_calls_async(0).await;
        assert_eq!(evaluation.state, PolicyMemberState::Evaluated);
        assert_eq!(
            world.outcomes(&evaluation),
            ["job-1: re-run", "job-5: NoContentKey"]
        );
        assert_eq!(
            (
                evaluation.answered_job_count,
                evaluation.waiting_job_count,
                evaluation.kept_job_count
            ),
            (4, 1, 1)
        );
        assert_eq!(counts(&evaluation, PolicyKind::Swap), Some((2, 1, 0, 0)));
        assert_eq!(
            evaluation.earliest_deadline,
            head.job_expiry.map(|expiry| expiry.earliest)
        );
        Ok(())
    }

    /// The columns of [`flagged_by`].
    const CELL_COLUMNS: [&str; 14] = [
        "job_id",
        "benchmark_id",
        "benchmark_type",
        "metric",
        "value",
        "value_stddev",
        "unit",
        "failed",
        "failure_source",
        "failure_code",
        "observation_max_swap_bytes",
        "model_descriptor",
        "model_params_total_millions",
        "model_params_active_millions",
    ];

    /// The rows of a `decode_throughput_512_100` run of job-perf that `kind`'s
    /// check flags. A measured row records [`MODEL`] with no parameter counts.
    fn flagged_by(kind: PolicyKind) -> Vec<Value> {
        let measured = |metric: &str, unit: &str, value: f64, stddev: f64, swap: u32| {
            json!([
                "job-perf",
                "decode_throughput_512_100",
                "decode_throughput",
                metric,
                value,
                stddev,
                unit,
                false,
                null,
                null,
                swap,
                MODEL,
                null,
                null
            ])
        };
        match kind {
            PolicyKind::Swap => vec![measured("decode", "tokens/sec", 100.0, 1.0, 4096)],
            PolicyKind::Spread => vec![measured("decode", "tokens/sec", 100.0, 10.0, 0)],
            PolicyKind::TtftOverE2e => vec![
                measured("ttft", "ms", 200.0, 1.0, 0),
                measured("end_to_end_latency", "ms", 100.0, 1.0, 0),
            ],
            PolicyKind::ModelParams => vec![measured("decode", "tokens/sec", 100.0, 1.0, 0)],
            PolicyKind::Failed | PolicyKind::SmallerFailed => vec![json!([
                "job-perf",
                "decode_throughput_512_100",
                "decode_throughput",
                "decode",
                null,
                null,
                "tokens/sec",
                true,
                "client",
                "runtime_error",
                null,
                null,
                null,
                null
            ])],
        }
    }

    /// No policy judges an eval: a text eval's, a VL eval's and an ASR eval's
    /// cells, typed so by their plan (the ASR eval's id has no eval prefix), are
    /// never read, so none is answered, waits, is flagged, re-run or marked, or
    /// keeps a job state, while each check flags the decode cell beside them, in
    /// the preview and in the pass. A larger clean decode cell is its family's.
    #[rstest::rstest]
    #[case::swap(PolicyKind::Swap, true)]
    #[case::spread(PolicyKind::Spread, true)]
    #[case::failed(PolicyKind::Failed, true)]
    #[case::ttft_over_e2e(PolicyKind::TtftOverE2e, true)]
    #[case::model_params(PolicyKind::ModelParams, false)]
    #[case::smaller_failed(PolicyKind::SmallerFailed, false)]
    #[tokio::test]
    async fn no_policy_judges_an_eval(
        #[case] kind: PolicyKind,
        #[case] reruns: bool,
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let cells = [
            (
                "job-perf",
                "decode_throughput_512_100",
                BenchmarkType::DecodeThroughput,
            ),
            (
                "job-larger",
                "decode_throughput_1024_100",
                BenchmarkType::DecodeThroughput,
            ),
            ("job-eval", "eval_math_500", BenchmarkType::Eval),
            ("job-vlm", "vlm_eval_mmstar", BenchmarkType::VlmEval),
            ("job-asr", "librispeech_wer", BenchmarkType::AsrEval),
        ];
        let head = PlanManifest {
            benchmark_types: cells
                .iter()
                .map(|(_, id, benchmark_type)| Ok((BenchmarkId::try_new(*id)?, *benchmark_type)))
                .collect::<anyhow::Result<_>>()?,
            ..world.plan(HEAD, &cells.map(|(job_id, ..)| (job_id, None)))?
        };
        world.put(std::slice::from_ref(&head)).await?;
        world
            .source(&cells.map(|(job_id, benchmark, _)| (job_id, benchmark)))
            .await?;
        let evals: Vec<Mock<'_>> = futures::stream::iter(["job-eval", "job-vlm", "job-asr"])
            .then(|job_id| {
                world.duckdb.mock_async(move |when, then| {
                    when.method(POST).body_includes(format!("'{job_id}'"));
                    then.status(500);
                })
            })
            .collect()
            .await;
        let larger = json!([
            "job-larger",
            "decode_throughput_1024_100",
            "decode_throughput",
            "decode",
            100.0,
            1.0,
            "tokens/sec",
            false,
            null,
            null,
            0,
            MODEL,
            350,
            350
        ]);
        let rows: Vec<Value> = flagged_by(kind).into_iter().chain([larger]).collect();
        let answered = world
            .duckdb
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/api/query")
                    .body_includes("'job-perf'");
                then.status(200)
                    .json_body(json!({ "columns": CELL_COLUMNS, "rows": rows }));
            })
            .await;
        let table = ModelParamTable::from_toml("\"LFM2-700M\" = 742")?;
        let catalog: HashMap<BenchmarkId, Benchmark> = [
            ("decode_throughput_512_100", 512),
            ("decode_throughput_1024_100", 1024),
        ]
        .into_iter()
        .map(|(id, prefill_tokens)| {
            let definition = format!(
                "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = \
                 {prefill_tokens}\nparameter_decode_tokens = 100"
            );
            Ok((
                BenchmarkId::try_new(id)?,
                Benchmark::from_toml(id, &definition)?,
            ))
        })
        .collect::<anyhow::Result<_>>()?;
        let stored = [world.on(kind, kind.max_reruns().min(1))];
        let duckdb = world.config().duckdb.context("the mock pipette-duckdb")?;
        let results = ResultsDb::new(reqwest::Client::new(), duckdb)?;
        let none = GroupPolicyState::default();
        let reading = |preview| Pass {
            state: &none,
            todo: &*world.stores.todo,
            submissions: &*world.stores.submissions,
            preview,
            table: Some(&table),
            catalog: Some(&catalog),
            waiting: &[],
            job_states: &[],
            bodies: None,
        };
        let (plans, group, campaign) = (&*world.stores.plans, &world.group, &world.campaign);
        let preview = evaluate(
            plans,
            Some(&results),
            group,
            campaign,
            &stored,
            reading(true),
        )
        .await?;
        let pass = evaluate(
            plans,
            Some(&results),
            group,
            campaign,
            &stored,
            reading(false),
        )
        .await?;
        answered.assert_calls_async(2).await;
        futures::future::join_all(evals.iter().map(|eval| eval.assert_calls_async(0))).await;
        let perf = BTreeSet::from([job("job-larger"), job("job-perf")]);
        [&preview, &pass].iter().for_each(|evaluation| {
            let named: BTreeSet<JobId> = evaluation
                .flagged
                .keys()
                .chain(&evaluation.rerun_job_ids)
                .chain(evaluation.marked.keys())
                .chain(evaluation.skipped.keys())
                .chain(evaluation.reruns.keys())
                .chain(&evaluation.decided)
                .chain(evaluation.read.iter().map(|job_state| &job_state.job_id))
                .chain(
                    evaluation
                        .smaller_failed
                        .iter()
                        .flat_map(|found| [&found.job_id, &found.clean_job_id]),
                )
                .cloned()
                .collect();
            assert!(named.is_subset(&perf), "{kind}: {named:?}");
            assert_eq!(
                (
                    evaluation.state,
                    evaluation.answered_job_count,
                    evaluation.waiting_job_count,
                    counts(evaluation, kind).map(|(flagged, ..)| flagged),
                    evaluation.rerun_job_ids.clone(),
                ),
                (
                    PolicyMemberState::Evaluated,
                    2,
                    0,
                    Some(1),
                    reruns
                        .then(|| job("job-perf"))
                        .into_iter()
                        .collect::<BTreeSet<_>>(),
                ),
                "{kind}"
            );
        });
        let kept: Option<BTreeSet<JobId>> = pass.job_states.map(|job_states| {
            job_states
                .into_iter()
                .map(|job_state| job_state.job_id)
                .collect()
        });
        assert_eq!(
            (kept, pass.record.map(|record| record.triggered)),
            (Some(perf), Some(BTreeSet::from([kind])))
        );
        Ok(())
    }

    /// The mock answers no query, so a member that reads rows reads unreadable.
    #[tokio::test]
    async fn a_member_is_read_no_further_than_its_campaign_and_plan_allow() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let plan = |plan_id: &str, status: PlanStatus| -> anyhow::Result<PlanManifest> {
            Ok(PlanManifest {
                status,
                ..world.plan(plan_id, &[("job-1", None)])?
            })
        };
        world
            .put(&[
                plan(HEAD, PlanStatus::Active)?,
                plan("plan-creating", PlanStatus::Creating)?,
                plan("plan-cancelling", PlanStatus::Cancelling)?,
                plan("plan-cancelled", PlanStatus::Cancelled)?,
                plan("plan-stopped", PlanStatus::Active)?,
                PlanManifest {
                    source_sha256: None,
                    ..plan("plan-bodies", PlanStatus::Active)?
                },
            ])
            .await?;
        world
            .stores
            .plans
            .write_cancel_marker(&PlanId::try_new("plan-stopped")?)
            .await?;
        let running = |plan_id: &str| -> anyhow::Result<Campaign> {
            Ok(Campaign {
                current_plan_id: Some(PlanId::try_new(plan_id)?),
                ..world.campaign.clone()
            })
        };
        let cases = [
            (
                Campaign {
                    owner: CampaignOwner::User {
                        user_id: UserId::try_new("user_other")?,
                    },
                    ..world.campaign.clone()
                },
                PolicyMemberState::OtherOwner,
            ),
            (
                Campaign {
                    current_plan_id: None,
                    ..world.campaign.clone()
                },
                PolicyMemberState::NoPlan,
            ),
            (
                Campaign {
                    state: CampaignState::Archived,
                    ..world.campaign.clone()
                },
                PolicyMemberState::NoPlan,
            ),
            (
                Campaign {
                    state: CampaignState::Locked,
                    ..world.campaign.clone()
                },
                PolicyMemberState::NoPlan,
            ),
            (running("plan-missing")?, PolicyMemberState::NoPlan),
            (running("plan-bodies")?, PolicyMemberState::NoPlan),
            (running("plan-creating")?, PolicyMemberState::NotReady),
            (running("plan-cancelling")?, PolicyMemberState::Cancelled),
            (running("plan-cancelled")?, PolicyMemberState::Cancelled),
            (running("plan-stopped")?, PolicyMemberState::Cancelled),
            (world.campaign.clone(), PolicyMemberState::Unreadable),
        ];
        let stored = [world.on(PolicyKind::Swap, 3)];
        let read = futures::future::try_join_all(
            cases
                .iter()
                .map(|(campaign, _)| world.read(campaign, &stored)),
        )
        .await?;
        assert_eq!(
            read.iter()
                .map(|evaluation| evaluation.state)
                .collect::<Vec<_>>(),
            cases.iter().map(|(_, state)| *state).collect::<Vec<_>>()
        );
        // After a failed read, a member that needs one is not read.
        let none = GroupPolicyState::default();
        let preview = Pass {
            state: &none,
            todo: &*world.stores.todo,
            submissions: &*world.stores.submissions,
            preview: true,
            table: None,
            catalog: None,
            waiting: &[],
            job_states: &[],
            bodies: None,
        };
        let unread = evaluate(
            &*world.stores.plans,
            None,
            &world.group,
            &world.campaign,
            &stored,
            preview,
        );
        assert_eq!(unread.await?.state, PolicyMemberState::Unreadable);
        Ok(())
    }

    /// A check's attempts are the runs in a row it flags, back along the chain.
    /// The count ends at a run it does not flag, at a mark, or once the re-runs
    /// are spent, and the rows of the run before are then not read. A swap
    /// policy with no re-runs marks the job at once.
    #[rstest::rstest]
    #[case::none(0, run("job-b", &[PolicyKind::Swap]), &["job-c: flagged on 1 run in a row, 0 re-runs spent."], 0)]
    #[case::spent_at_job_b(1, run("job-b", &[PolicyKind::Swap]), &["job-c: flagged on 2 runs in a row, 1 re-run spent."], 0)]
    #[case::spent_at_job_a(2, run("job-b", &[PolicyKind::Swap]), &["job-c: flagged on 3 runs in a row, 2 re-runs spent."], 1)]
    #[case::left(3, run("job-b", &[PolicyKind::Swap]), &["job-c: re-run"], 1)]
    #[case::not_flagged(1, run("job-b", &[]), &["job-c: re-run"], 0)]
    #[case::mark(1, mark("job-b"), &["job-c: re-run"], 0)]
    #[tokio::test]
    async fn flagged_runs_in_a_row_count_back_along_the_chain(
        #[case] max_reruns: u32,
        #[case] job_b: Vec<Value>,
        #[case] outcomes: &[&str],
        #[case] job_a_reads: usize,
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan("plan-2", &[("job-b", Some("job-a"))])?,
                world.plan(HEAD, &[("job-c", Some("job-b"))])?,
            ])
            .await?;
        let swap = [PolicyKind::Swap];
        world.answer(&[("job-c", run("job-c", &swap))]).await;
        world.answer(&[("job-b", job_b)]).await;
        let job_a = world.answer(&[("job-a", run("job-a", &swap))]).await;
        let evaluation = world
            .read(&world.campaign, &[world.on(PolicyKind::Swap, max_reruns)])
            .await?;
        assert_eq!(world.outcomes(&evaluation), outcomes);
        job_a.assert_calls_async(job_a_reads).await;
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_holders_read_makes_the_job_wait() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan("plan-2", &[("job-b", Some("job-a"))])?,
                world.plan(HEAD, &[("job-c", Some("job-b")), ("job-d", None)])?,
            ])
            .await?;
        world
            .db
            .execute("ALTER TABLE job_plan_links RENAME TO job_plan_links_gone")
            .await?;
        let swap = [PolicyKind::Swap];
        world
            .answer(&[
                ("job-c", run("job-c", &swap)),
                ("job-d", run("job-d", &swap)),
            ])
            .await;
        world.answer(&[("job-b", run("job-b", &swap))]).await;
        let evaluation = world
            .read(&world.campaign, &[world.on(PolicyKind::Swap, 3)])
            .await?;
        assert_eq!(
            world.outcomes(&evaluation),
            ["job-c: ChainReadFailed", "job-d: re-run"]
        );
        Ok(())
    }

    /// A failed holders read stops only the checks that still need the walk:
    /// swap, its re-run spent at job-b, marks job-c, while spread, off at its
    /// defaults, waits.
    #[tokio::test]
    async fn a_failed_holders_read_stops_only_the_checks_still_open() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan("plan-2", &[("job-b", Some("job-a"))])?,
                world.plan(HEAD, &[("job-c", Some("job-b"))])?,
            ])
            .await?;
        world
            .db
            .execute("ALTER TABLE job_plan_links RENAME TO job_plan_links_gone")
            .await?;
        let both = [PolicyKind::Swap, PolicyKind::Spread];
        world.answer(&[("job-c", run("job-c", &both))]).await;
        world.answer(&[("job-b", run("job-b", &both))]).await;
        let evaluation = world
            .read(&world.campaign, &[world.on(PolicyKind::Swap, 1)])
            .await?;
        assert_eq!(
            world.outcomes(&evaluation),
            ["job-c: flagged on 2 runs in a row, 1 re-run spent."]
        );
        assert_eq!(counts(&evaluation, PolicyKind::Swap), Some((1, 0, 1, 0)));
        assert_eq!(counts(&evaluation, PolicyKind::Spread), Some((1, 0, 0, 0)));
        Ok(())
    }

    /// A run held by a plan that decodes follows that plan. A run held only by
    /// one that does not leaves its job as it is, and the other jobs go on.
    #[tokio::test]
    async fn a_plan_that_does_not_decode_is_read_past_where_another_holds_the_run()
    -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan("plan-2", &[("job-b", Some("job-a"))])?,
                world.plan(
                    "plan-broken",
                    &[("job-b", Some("job-a")), ("job-e", Some("job-z"))],
                )?,
                world.plan(HEAD, &[("job-c", Some("job-b")), ("job-f", Some("job-e"))])?,
            ])
            .await?;
        world
            .db
            .execute("UPDATE plans SET body = 'not a plan' WHERE plan_id = 'plan-broken'")
            .await?;
        let swap = [PolicyKind::Swap];
        world
            .answer(&[
                ("job-c", run("job-c", &swap)),
                ("job-f", run("job-f", &swap)),
            ])
            .await;
        world
            .answer(&[
                ("job-b", run("job-b", &swap)),
                ("job-e", run("job-e", &swap)),
            ])
            .await;
        world.answer(&[("job-a", run("job-a", &swap))]).await;
        let evaluation = world
            .read(&world.campaign, &[world.on(PolicyKind::Swap, 2)])
            .await?;
        assert_eq!(
            world.outcomes(&evaluation),
            [
                "job-c: flagged on 3 runs in a row, 2 re-runs spent.",
                "job-f: UnreadablePlan"
            ]
        );
        Ok(())
    }

    /// Once their re-runs are spent, swap and spread mark a job failed, the
    /// first of them that flags it naming the mark, and ttft_over_e2e keeps
    /// its result.
    #[tokio::test]
    async fn a_spent_budget_marks_a_swap_or_a_spread_and_keeps_the_others() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan(
                    "plan-1",
                    &[("job-a", None), ("job-b", None), ("job-x", None)],
                )?,
                world.plan(
                    HEAD,
                    &[
                        ("job-c", Some("job-a")),
                        ("job-d", Some("job-b")),
                        ("job-e", Some("job-x")),
                    ],
                )?,
            ])
            .await?;
        let (both, spread) = ([PolicyKind::Swap, PolicyKind::Spread], [PolicyKind::Spread]);
        world
            .answer(&[
                ("job-c", run("job-c", &both)),
                ("job-d", run("job-d", &spread)),
                ("job-e", ttft_over("job-e")),
            ])
            .await;
        world
            .answer(&[
                ("job-a", run("job-a", &both)),
                ("job-b", run("job-b", &spread)),
                ("job-x", ttft_over("job-x")),
            ])
            .await;
        let stored = [
            world.on(PolicyKind::Swap, 1),
            world.on(PolicyKind::Spread, 1),
            world.on(PolicyKind::TtftOverE2e, 1),
        ];
        let evaluation = world.read(&world.campaign, &stored).await?;
        assert_eq!(
            world.outcomes(&evaluation),
            [
                "job-c: flagged on 2 runs in a row, 1 re-run spent.",
                "job-d: spread: flagged on 2 runs in a row, 1 re-run spent."
            ]
        );
        assert_eq!(evaluation.kept_job_count, 1);
        assert_eq!(
            evaluation.kinds,
            BTreeSet::from([PolicyKind::Swap, PolicyKind::Spread])
        );
        assert_eq!(counts(&evaluation, PolicyKind::Swap), Some((1, 0, 1, 0)));
        assert_eq!(counts(&evaluation, PolicyKind::Spread), Some((2, 0, 2, 0)));
        assert_eq!(
            counts(&evaluation, PolicyKind::TtftOverE2e),
            Some((1, 0, 0, 1))
        );
        Ok(())
    }

    /// Spread marks a cell failed once it flags the cell on 1 + `max_reruns`
    /// runs in a row, at the group's own `percent` and `max_reruns`. At 8% with
    /// 1 re-run, job-c, at 10% on its last two runs, is marked, job-d, whose
    /// run before was at 6%, is re-run, and job-e, at 7%, is not flagged. With
    /// 2 re-runs job-c is marked on its third run in a row; at 5%, job-d is
    /// marked and job-e re-run.
    #[rstest::rstest]
    #[case::one_rerun(
        8.0,
        1,
        &["job-c: spread: flagged on 2 runs in a row, 1 re-run spent.", "job-d: re-run"],
        (2, 1, 1, 0)
    )]
    #[case::two_reruns(
        8.0,
        2,
        &["job-c: spread: flagged on 3 runs in a row, 2 re-runs spent.", "job-d: re-run"],
        (2, 1, 1, 0)
    )]
    #[case::five_percent(
        5.0,
        1,
        &[
            "job-c: spread: flagged on 2 runs in a row, 1 re-run spent.",
            "job-d: spread: flagged on 2 runs in a row, 1 re-run spent.",
            "job-e: re-run",
        ],
        (3, 1, 2, 0)
    )]
    #[tokio::test]
    async fn spread_marks_a_cell_past_its_reruns_at_the_groups_settings(
        #[case] percent: f64,
        #[case] max_reruns: u32,
        #[case] outcomes: &[&str],
        #[case] spread: (usize, usize, usize, usize),
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-0", &[("job-z", None)])?,
                world.plan("plan-1", &[("job-a", Some("job-z")), ("job-b", None)])?,
                world.plan(
                    HEAD,
                    &[
                        ("job-c", Some("job-a")),
                        ("job-d", Some("job-b")),
                        ("job-e", None),
                    ],
                )?,
            ])
            .await?;
        world
            .answer(&[
                ("job-c", spread_of("job-c", 10.0)),
                ("job-d", spread_of("job-d", 10.0)),
                ("job-e", spread_of("job-e", 7.0)),
            ])
            .await;
        world
            .answer(&[
                ("job-a", spread_of("job-a", 10.0)),
                ("job-b", spread_of("job-b", 6.0)),
            ])
            .await;
        world.answer(&[("job-z", spread_of("job-z", 10.0))]).await;
        let stored = [CampaignGroupPolicy {
            settings: PolicySettings {
                percent: Some(percent),
                ..PolicySettings::default()
            },
            ..world.on(PolicyKind::Spread, max_reruns)
        }];
        let evaluation = world.read(&world.campaign, &stored).await?;
        assert_eq!(world.outcomes(&evaluation), outcomes);
        assert_eq!(counts(&evaluation, PolicyKind::Spread), Some(spread));
        Ok(())
    }

    /// The chain's earlier runs are followed by their links alone once no
    /// check counts: only the first one's rows are read.
    #[rstest::rstest]
    #[case::fifteen(15, &["job-c: re-run"], (1, 1, 0, 0), (1, 1, 0, 0))]
    #[case::sixteen(16, &["job-c: its re-run chain has 16 runs."], (1, 0, 1, 0), (1, 0, 1, 0))]
    #[tokio::test]
    async fn a_job_whose_chain_has_16_runs_is_not_rerun(
        #[case] runs: usize,
        #[case] outcomes: &[&str],
        #[case] swap: (usize, usize, usize, usize),
        #[case] spread: (usize, usize, usize, usize),
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let earlier: Vec<String> = (1..runs).map(|n| format!("run-{n}")).collect();
        let history: Vec<(&str, Option<&str>)> = earlier
            .iter()
            .zip(
                earlier
                    .iter()
                    .skip(1)
                    .map(|before| Some(before.as_str()))
                    .chain([None]),
            )
            .map(|(job_id, before)| (job_id.as_str(), before))
            .collect();
        world
            .put(&[
                world.plan("plan-history", &history)?,
                world.plan(HEAD, &[("job-c", Some("run-1"))])?,
            ])
            .await?;
        world
            .answer(&[(
                "job-c",
                run("job-c", &[PolicyKind::Swap, PolicyKind::Spread]),
            )])
            .await;
        world.answer(&[("run-1", run("run-1", &[]))]).await;
        let stored = [
            world.on(PolicyKind::Swap, 3),
            world.on(PolicyKind::Spread, 3),
        ];
        let evaluation = world.read(&world.campaign, &stored).await?;
        assert_eq!(world.outcomes(&evaluation), outcomes);
        assert_eq!(counts(&evaluation, PolicyKind::Swap), Some(swap));
        assert_eq!(counts(&evaluation, PolicyKind::Spread), Some(spread));
        Ok(())
    }

    /// The policies that are on decide together, and each check alone counts
    /// the same whether it is on or off. Swap has spent its re-runs and spread,
    /// at its defaults, has some left: the job is re-run while spread is on,
    /// and marked once it is off. Spread off, waiting for an earlier run's
    /// rows, does not hold up the mark.
    #[rstest::rstest]
    #[case::spread_on(1, true, run("job-a", &[PolicyKind::Swap]), &["job-c: re-run"], (1, 1, 0, 0))]
    #[case::spread_off(1, false, run("job-a", &[PolicyKind::Swap]), &["job-c: flagged on 2 runs in a row, 1 re-run spent."], (1, 1, 0, 0))]
    #[case::spread_off_waiting(0, false, Vec::new(), &["job-c: flagged on 1 run in a row, 0 re-runs spent."], (1, 0, 0, 0))]
    #[tokio::test]
    async fn the_policies_that_are_on_decide_together_and_each_check_counts_alone(
        #[case] swap_reruns: u32,
        #[case] spread_on: bool,
        #[case] job_a: Vec<Value>,
        #[case] outcomes: &[&str],
        #[case] spread: (usize, usize, usize, usize),
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan(HEAD, &[("job-c", Some("job-a"))])?,
            ])
            .await?;
        world
            .answer(&[(
                "job-c",
                run("job-c", &[PolicyKind::Swap, PolicyKind::Spread]),
            )])
            .await;
        world.answer(&[("job-a", job_a)]).await;
        let stored = [
            world.on(PolicyKind::Swap, swap_reruns),
            CampaignGroupPolicy {
                enabled: spread_on,
                ..PolicyKind::Spread.defaults(world.group.group_id.clone())
            },
        ];
        let evaluation = world.read(&world.campaign, &stored).await?;
        assert_eq!(world.outcomes(&evaluation), outcomes);
        assert_eq!(counts(&evaluation, PolicyKind::Swap), Some((1, 0, 1, 0)));
        assert_eq!(counts(&evaluation, PolicyKind::Spread), Some(spread));
        Ok(())
    }

    /// The pass walks a chain only for the policies that are on: spread, off,
    /// follows job-c back to job-a in the preview, and not in the pass, which
    /// marks job-c as the preview does.
    #[tokio::test]
    async fn the_pass_walks_chains_only_for_the_policies_that_are_on() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan(HEAD, &[("job-c", Some("job-a"))])?,
            ])
            .await?;
        let both = [PolicyKind::Swap, PolicyKind::Spread];
        world.answer(&[("job-c", run("job-c", &both))]).await;
        let job_a = world.answer(&[("job-a", run("job-a", &both))]).await;
        let stored = [world.on(PolicyKind::Swap, 0)];
        let preview = world.read(&world.campaign, &stored).await?;
        let pass = world
            .round(&world.campaign, &stored, Some(&GroupPolicyState::default()))
            .await?;
        job_a.assert_calls_async(1).await;
        let marked = ["job-c: flagged on 1 run in a row, 0 re-runs spent."];
        assert_eq!(world.outcomes(&preview), marked);
        assert_eq!(world.outcomes(&pass), marked);
        assert_eq!(counts(&preview, PolicyKind::Spread), Some((1, 1, 0, 0)));
        assert_eq!(counts(&pass, PolicyKind::Spread), Some((0, 0, 0, 0)));
        Ok(())
    }

    /// A run held by a newer plan that names no earlier run, as a revision
    /// that took the job from an older plan holds it, is followed by the
    /// holder that names one.
    #[tokio::test]
    async fn the_walk_follows_the_holder_that_names_the_earlier_run() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan("plan-2", &[("job-b", Some("job-a"))])?,
                world.plan("plan-3", &[("job-b", None)])?,
                world.plan(HEAD, &[("job-c", Some("job-b"))])?,
            ])
            .await?;
        let swap = [PolicyKind::Swap];
        world.answer(&[("job-c", run("job-c", &swap))]).await;
        world.answer(&[("job-b", run("job-b", &swap))]).await;
        world.answer(&[("job-a", run("job-a", &swap))]).await;
        let evaluation = world
            .read(&world.campaign, &[world.on(PolicyKind::Swap, 2)])
            .await?;
        assert_eq!(
            world.outcomes(&evaluation),
            ["job-c: flagged on 3 runs in a row, 2 re-runs spent."]
        );
        Ok(())
    }

    /// The failed check re-runs a device failure that a retry may fix, and
    /// keeps a timeout, which the pass records as decided; the re-run job
    /// triggers the policy.
    #[tokio::test]
    async fn a_timeout_is_kept_and_decided_and_a_runtime_error_rerun() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world
            .put(&[world.plan(HEAD, &[("job-1", None), ("job-2", None)])?])
            .await?;
        world
            .answer(&[
                ("job-1", failed_run("job-1", "client", "runtime_error")),
                ("job-2", failed_run("job-2", "client", "attempt_timeout")),
            ])
            .await;
        let stored = [world.on(PolicyKind::Failed, 0)];
        let evaluation = world
            .round(&world.campaign, &stored, Some(&GroupPolicyState::default()))
            .await?;
        assert_eq!(world.outcomes(&evaluation), ["job-1: re-run"]);
        assert_eq!(evaluation.kinds, BTreeSet::from([PolicyKind::Failed]));
        let job_states: Vec<(JobId, Vec<PolicyKind>, bool)> = evaluation
            .job_states
            .context("the round's job states")?
            .into_iter()
            .map(|v| (v.job_id, v.flagged.into_iter().collect(), v.decided))
            .collect();
        assert_eq!(
            job_states,
            [
                (job("job-1"), vec![PolicyKind::Failed], false),
                (job("job-2"), vec![], true)
            ]
        );
        assert_eq!(
            evaluation.record.map(|record| record.triggered),
            Some(BTreeSet::from([PolicyKind::Failed]))
        );
        Ok(())
    }

    /// Failed counts the runs in a row it flags, any listed code, and re-runs
    /// while that count is within the limit of the last run's code: with
    /// device_not_ready at 1 and runtime_error at 2, a readiness failure is
    /// re-run, its crash re-run again, and a readiness failure after that kept,
    /// as is a readiness failure after one crash.
    #[rstest::rstest]
    #[case::not_ready(&["device_not_ready"], &[1])]
    #[case::then_crashed(&["device_not_ready", "runtime_error"], &[2])]
    #[case::then_not_ready(&["device_not_ready", "runtime_error", "device_not_ready"], &[])]
    #[case::not_ready_after_a_crash(&["runtime_error", "device_not_ready"], &[])]
    #[tokio::test]
    async fn failed_reruns_within_the_last_runs_code_limit(
        #[case] codes: &[&str],
        #[case] attempts: &[u32],
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let jobs: Vec<String> = (1..=codes.len()).map(|n| format!("job-{n}")).collect();
        let plans = jobs
            .iter()
            .enumerate()
            .map(|(at, job_id)| {
                let plan_id = if at + 1 == jobs.len() {
                    HEAD.to_owned()
                } else {
                    format!("plan-{at}")
                };
                let of = at.checked_sub(1).map(|before| jobs[before].as_str());
                world.plan(&plan_id, &[(job_id.as_str(), of)])
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        world.put(&plans).await?;
        let answering = &world;
        let _answers: Vec<Mock<'_>> = futures::stream::iter(jobs.iter().zip(codes))
            .then(|(job_id, code)| async move {
                answering
                    .answer(&[(job_id.as_str(), failed_run(job_id, "client", code))])
                    .await
            })
            .collect()
            .await;
        let failed = CampaignGroupPolicy {
            settings: PolicySettings {
                codes: Some(
                    [
                        (FailureCode::DeviceNotReady, 1),
                        (FailureCode::RuntimeError, 2),
                    ]
                    .into(),
                ),
                ..PolicySettings::default()
            },
            ..world.on(PolicyKind::Failed, 0)
        };
        let evaluation = world.read(&world.campaign, &[failed]).await?;
        let filed: Vec<u32> = evaluation
            .reruns
            .values()
            .flatten()
            .map(|rerun| rerun.attempt)
            .collect();
        assert_eq!(filed, attempts);
        let rerun = usize::from(!attempts.is_empty());
        assert_eq!(
            counts(&evaluation, PolicyKind::Failed),
            Some((1, rerun, 0, 1 - rerun))
        );
        Ok(())
    }

    /// A job answered with no rows waits: the next round reads it alone, the
    /// job it kept left out, and counts it again.
    #[tokio::test]
    async fn a_job_without_rows_is_read_again_and_counted() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        world
            .put(&[world.plan(HEAD, &[("job-1", None), ("job-2", None)])?])
            .await?;
        let both = world
            .answer(&[("job-1", run("job-1", &[])), ("job-2", Vec::new())])
            .await;
        let alone = world.answer(&[("job-2", Vec::new())]).await;
        let stored = [world.on(PolicyKind::Swap, 3)];
        let first = world
            .round(&world.campaign, &stored, Some(&GroupPolicyState::default()))
            .await?;
        let record = first.record.clone().context("the first round's record")?;
        let job_states = first
            .job_states
            .clone()
            .context("the first round's job states")?;
        world.keep(&record, &job_states).await?;
        let second = world.round(&world.campaign, &stored, Some(&record)).await?;
        both.assert_calls_async(1).await;
        alone.assert_calls_async(1).await;
        assert_eq!((first.waiting_job_count, second.waiting_job_count), (1, 1));
        let decided: Vec<(&JobId, bool)> =
            job_states.iter().map(|v| (&v.job_id, v.decided)).collect();
        assert_eq!(decided, [(&job("job-1"), true)]);
        assert_eq!(
            (second.record, second.job_states),
            (Some(record), Some(job_states))
        );
        Ok(())
    }

    /// The stored body of a decode_throughput success of `job_id`, at 100
    /// tokens/sec with a 1% spread, and a peak swap of `swap` bytes.
    fn decode_body(job_id: &str, swap: i64) -> Value {
        json!({
            "message_type": "success",
            "benchmark_id": "decode_throughput_512_100",
            "benchmark_type": "decode_throughput",
            "client_id": "ev1_a",
            "job_id": job_id,
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "mac mini",
            "device_form_factor": "desktop",
            "device_os_name": "macos",
            "device_os_version": "15.0",
            "device_chip_model": "m4",
            "device_ram_bytes": 16_000_000_000_i64,
            "decode_time_ms": 1000.0,
            "decode_time_ms_stddev": 10.0,
            "observation_max_swap_bytes": swap,
        })
    }

    /// The stored body of a decode_throughput failure of `job_id` that its
    /// device reports with `code`.
    fn failure_body(job_id: &str, code: &str) -> Value {
        json!({
            "message_type": "failure",
            "benchmark_id": "decode_throughput_512_100",
            "benchmark_type": "decode_throughput",
            "client_id": "ev1_a",
            "job_id": job_id,
            "submitted_at": "2026-01-01T00:00:00Z",
            "retriable": false,
            "failure_reason": "OOM",
            "failure_code": code,
        })
    }

    /// Only an unchanged body can supply a candidate's named rows on its
    /// second read, including when a replacement changes metadata alone.
    #[rstest::rstest]
    #[case::unchanged("unchanged")]
    #[case::changed_values("changed_values")]
    #[case::changed_metadata("changed_metadata")]
    #[case::missing("missing")]
    #[case::malformed("malformed")]
    #[tokio::test]
    async fn a_rerun_candidate_requires_the_same_body(#[case] change: &str) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let job_id = job("job-1");
        let original = decode_body(job_id.as_str(), 4096);
        let submissions = &*world.stores.submissions;
        submissions.write_incoming(&job_id, &original).await?;
        let id = BenchmarkId::try_new("decode_throughput_512_100")?;
        let benchmark = Benchmark::from_toml(
            id.as_str(),
            "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 512\n\
             parameter_decode_tokens = 100",
        )?;
        let catalog = HashMap::from([(id, benchmark)]);
        let first = body_rows(submissions, &catalog, vec![job_id.clone()]).await;
        let (fingerprint, rows) = first.get(&job_id).context("the first read gives rows")?;
        assert!(flags(&world.on(PolicyKind::Swap, 3), rows));
        match change {
            "unchanged" => {}
            "changed_values" => {
                submissions
                    .write_incoming(&job_id, &decode_body(job_id.as_str(), 0))
                    .await?;
            }
            "changed_metadata" => {
                let mut replacement = original.clone();
                replacement["submitted_at"] = json!("2026-01-01T00:00:01Z");
                submissions.write_incoming(&job_id, &replacement).await?;
            }
            "missing" => submissions.delete_incoming(&job_id).await?,
            "malformed" => submissions.write_incoming(&job_id, &json!({})).await?,
            other => anyhow::bail!("unknown body change: {other}"),
        }
        let second = named_rows(
            submissions,
            BodyReads {
                catalog: &catalog,
                stores: &world.stores,
            },
            vec![(job_id.clone(), *fingerprint)],
        )
        .await;
        assert_eq!(second.contains_key(&job_id), change == "unchanged");
        if let Some(rows) = second.get(&job_id) {
            assert!(flags(&world.on(PolicyKind::Swap, 3), rows));
        }
        Ok(())
    }

    /// A swap in a result's body files its re-run in the first round after
    /// the answer, before pipette-duckdb has its rows, and the re-run records
    /// the swap that the body reports.
    #[tokio::test]
    async fn a_swap_in_a_body_files_its_rerun_before_its_rows() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = [world.on(PolicyKind::Swap, 3)];
        world.turn_on(&stored).await?;
        let swapped = world.jobs.first().context("a job")?;
        world
            .submit(swapped, &decode_body(swapped.as_str(), 4096))
            .await?;
        world.maintain().await?;
        world.answer(&[(swapped.as_str(), Vec::new())]).await;
        let (evaluation, filing, counts) = world.pass(&stored).await?;
        assert_eq!(evaluation.from_body, BTreeSet::from([swapped.clone()]));
        assert!(matches!(
            filing.map(|filing| filing.outcome),
            Some(FilingOutcome::Advanced { .. })
        ));
        assert_eq!(
            (
                counts.body_reads,
                counts.body_reruns,
                counts.jobs_waiting_for_rows
            ),
            (2, 1, 0)
        );
        let reruns = world.reruns().await?;
        let recorded: Vec<(&JobId, f64, Option<&str>)> = reruns
            .iter()
            .map(|rerun| (&rerun.rerun_of, rerun.before, rerun.model_key.as_deref()))
            .collect();
        assert_eq!(recorded, [(swapped, 4096.0, None)]);
        Ok(())
    }

    /// A body that no check flags leaves its job waiting, undecided and with
    /// no job state; once its rows land and a check flags them, the rows
    /// re-run it.
    #[tokio::test]
    async fn a_clean_body_waits_for_the_rows_that_act() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        world.put(&[world.plan(HEAD, &[("job-1", None)])?]).await?;
        world
            .submit(&job("job-1"), &decode_body("job-1", 0))
            .await?;
        world.answer(&[("job-1", Vec::new())]).await;
        let stored = [world.on(PolicyKind::Swap, 3)];
        let first = world.round_with_bodies(&stored, &[]).await?;
        assert_eq!((first.body_reads, first.waiting_job_count), (1, 1));
        assert!(first.rerun_job_ids.is_empty() && first.from_body.is_empty());
        assert!(first.decided.is_empty());
        assert_eq!(first.job_states, Some(Vec::new()));
        world.duckdb.reset_async().await;
        world
            .answer(&[("job-1", run("job-1", &[PolicyKind::Swap]))])
            .await;
        let second = world.round_with_bodies(&stored, &[]).await?;
        assert_eq!((second.body_reads, second.waiting_job_count), (0, 0));
        assert_eq!(second.rerun_job_ids, BTreeSet::from([job("job-1")]));
        assert!(second.from_body.is_empty());
        Ok(())
    }

    /// A body whose outcome is a mark, a keep or a skip acts on nothing: the
    /// job waits, undecided, for the rows that the same outcome acts on.
    #[rstest::rstest]
    #[case::mark(PolicyKind::Swap, 0, true)]
    #[case::keep(PolicyKind::Failed, 0, true)]
    #[case::skip(PolicyKind::Swap, 3, false)]
    #[tokio::test]
    async fn a_body_acts_only_by_a_rerun(
        #[case] kind: PolicyKind,
        #[case] max_reruns: u32,
        #[case] keyed: bool,
    ) -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        let head = world.plan(HEAD, &[("job-1", None)])?;
        let head = PlanManifest {
            content_keys: head.content_keys.into_iter().filter(|_| keyed).collect(),
            ..head
        };
        world.put(&[head]).await?;
        let on = world.on(kind, max_reruns);
        let policy = CampaignGroupPolicy {
            settings: PolicySettings {
                codes: on
                    .settings
                    .codes
                    .as_ref()
                    .map(|_| BTreeMap::from([(FailureCode::RuntimeError, max_reruns)])),
                ..on.settings.clone()
            },
            ..on
        };
        let (body, rows) = if kind == PolicyKind::Failed {
            (
                failure_body("job-1", "runtime_error"),
                failed_run("job-1", "client", "runtime_error"),
            )
        } else {
            (decode_body("job-1", 4096), run("job-1", &[kind]))
        };
        world.submit(&job("job-1"), &body).await?;
        world.answer(&[("job-1", Vec::new())]).await;
        let stored = [policy];
        let from_body = world.round_with_bodies(&stored, &[]).await?;
        assert_eq!((from_body.body_reads, from_body.waiting_job_count), (1, 1));
        assert!(from_body.rerun_job_ids.is_empty() && from_body.from_body.is_empty());
        assert!(from_body.marked.is_empty() && from_body.skipped.is_empty());
        assert!(from_body.decided.is_empty());
        world.duckdb.reset_async().await;
        world.answer(&[("job-1", rows)]).await;
        let from_rows = world.round_with_bodies(&stored, &[]).await?;
        assert!(from_rows.rerun_job_ids.is_empty());
        assert!(!from_rows.marked.is_empty() || from_rows.decided.contains(&job("job-1")));
        Ok(())
    }

    /// A re-run whose job has only a body is not judged, even as that body
    /// re-runs it again on its earlier run's rows.
    #[tokio::test]
    async fn a_rerun_is_not_judged_from_its_body() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        world
            .put(&[world.plan(HEAD, &[("job-2", Some("job-1"))])?])
            .await?;
        world
            .submit(&job("job-2"), &decode_body("job-2", 4096))
            .await?;
        world.answer(&[("job-2", Vec::new())]).await;
        world
            .answer(&[("job-1", run("job-1", &[PolicyKind::Swap]))])
            .await;
        let waiting = PolicyRerun {
            campaign_id: world.campaign.campaign_id.clone(),
            ..a_rerun(PolicyKind::Swap)?
        };
        let stored = [world.on(PolicyKind::Swap, 3)];
        let evaluation = world
            .round_with_bodies(&stored, std::slice::from_ref(&waiting))
            .await?;
        assert!(evaluation.judged.is_empty());
        assert_eq!(evaluation.from_body, BTreeSet::from([job("job-2")]));
        Ok(())
    }

    /// A held submission is not in the store the pass reads bodies from, so
    /// its job waits for its rows.
    #[tokio::test]
    async fn a_held_submission_reruns_nothing() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        world.put(&[world.plan(HEAD, &[("job-1", None)])?]).await?;
        world
            .stores
            .submissions
            .write_unverified(
                &pipette_plan_types::ClientId::try_new("ev1_a")?,
                &job("job-1"),
                &decode_body("job-1", 4096),
            )
            .await?;
        world.answer(&[("job-1", Vec::new())]).await;
        let stored = [world.on(PolicyKind::Swap, 3)];
        let evaluation = world.round_with_bodies(&stored, &[]).await?;
        assert_eq!(
            (evaluation.body_reads, evaluation.waiting_job_count),
            (1, 1)
        );
        assert!(evaluation.rerun_job_ids.is_empty());
        Ok(())
    }

    /// An eval's job is not among the jobs the policies read, so its body is
    /// not read.
    #[tokio::test]
    async fn an_evals_body_is_not_read() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        let head = PlanManifest {
            benchmark_types: BTreeMap::from([(
                BenchmarkId::try_new("eval_math_500")?,
                BenchmarkType::Eval,
            )]),
            ..world.plan(HEAD, &[("job-1", None)])?
        };
        world.put(&[head]).await?;
        world.source(&[("job-1", "eval_math_500")]).await?;
        world
            .submit(&job("job-1"), &decode_body("job-1", 4096))
            .await?;
        world.answer(&[]).await;
        let stored = [world.on(PolicyKind::Swap, 3)];
        let evaluation = world.round_with_bodies(&stored, &[]).await?;
        assert_eq!(
            (
                evaluation.answered_job_count,
                evaluation.body_reads,
                evaluation.waiting_job_count
            ),
            (0, 0, 0)
        );
        assert!(evaluation.rerun_job_ids.is_empty());
        Ok(())
    }

    /// A round with nothing newly answered reads no rows. Once a policy that
    /// is on is saved with other values, the next round reads every answered
    /// job again and keeps them under the new values.
    #[tokio::test]
    async fn a_save_reads_every_answered_job_again() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        world
            .put(&[world.plan(HEAD, &[("job-1", None), ("job-2", None)])?])
            .await?;
        let kept = GroupPolicyState {
            policies: vec![world.on(PolicyKind::Swap, 3)],
            rules: RULES,
            ..GroupPolicyState::default()
        };
        let decided = ["job-1", "job-2"]
            .map(|id| decided_state(id, &[]))
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
        world.keep(&kept, &decided).await?;
        // No query is answered yet, so a round that read rows would read
        // unreadable.
        let quiet = world
            .round(&world.campaign, &kept.policies, Some(&kept))
            .await?;
        assert_eq!(
            (
                quiet.state,
                quiet.record.as_ref(),
                quiet.job_states.as_ref()
            ),
            (PolicyMemberState::Evaluated, Some(&kept), Some(&decided))
        );
        let both = world
            .answer(&[("job-1", run("job-1", &[])), ("job-2", run("job-2", &[]))])
            .await;
        let saved = [world.on(PolicyKind::Swap, 4)];
        let read = world.round(&world.campaign, &saved, Some(&kept)).await?;
        both.assert_calls_async(1).await;
        assert_eq!(
            read.record,
            Some(GroupPolicyState {
                policies: saved.to_vec(),
                ..kept
            })
        );
        Ok(())
    }

    /// A member kept under older rules for what the policies do with a job
    /// they flag is read in full once under this build's: a spread cell the
    /// older rules kept with its re-runs spent is marked now, and the record
    /// keeps the current rules, under which later rounds trust its job states.
    #[tokio::test]
    async fn a_member_kept_under_older_rules_is_read_again() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        world
            .put(&[
                world.plan("plan-1", &[("job-a", None)])?,
                world.plan(HEAD, &[("job-c", Some("job-a")), ("job-d", None)])?,
            ])
            .await?;
        let stored = [world.on(PolicyKind::Spread, 1)];
        let older = GroupPolicyState {
            policies: stored.to_vec(),
            ..GroupPolicyState::default()
        };
        let decided = [
            decided_state("job-c", &[PolicyKind::Spread])?,
            decided_state("job-d", &[])?,
        ];
        world.keep(&older, &decided).await?;
        let spreads = [PolicyKind::Spread];
        world
            .answer(&[
                ("job-c", run("job-c", &spreads)),
                ("job-d", run("job-d", &[])),
            ])
            .await;
        world.answer(&[("job-a", run("job-a", &spreads))]).await;
        let read = world.round(&world.campaign, &stored, Some(&older)).await?;
        assert_eq!(
            world.outcomes(&read),
            ["job-c: spread: flagged on 2 runs in a row, 1 re-run spent."]
        );
        assert_eq!(read.record.map(|record| record.rules), Some(RULES));
        Ok(())
    }

    /// One filing into a head that still runs: it re-runs a swapped job, marks
    /// failed one whose swap re-runs are spent and one whose spread re-runs are
    /// spent, and carries the queued job and the leased one with their ids and
    /// the lease. Its change, its note and each mark's reason name the group
    /// and the policies, and the record keeps the filing.
    #[tokio::test]
    async fn a_filing_reruns_marks_and_carries_the_live_jobs() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let [swapped, spent, spread, queued, leased] =
            <[JobId; 5]>::try_from(world.jobs.clone())
                .map_err(|jobs| anyhow::anyhow!("five jobs, not {jobs:?}"))?;
        // A person's re-run gives two of them an earlier run.
        world.finish(&[&spent, &spread]).await?;
        campaign_admin::rerun_jobs(
            &world.stores,
            settings(),
            &world.campaign.campaign_id,
            BTreeSet::from([spent.clone(), spread.clone()]),
            None,
        )
        .await?;
        let head = world
            .manifest(&world.current().await?.current_plan_id.context("a head")?)
            .await?;
        let again = |earlier: &JobId| {
            head.rerun_of
                .iter()
                .find(|(_, of)| *of == earlier)
                .map(|(job_id, _)| job_id.clone())
                .context("the re-run")
        };
        let (spent_again, spread_again) = (again(&spent)?, again(&spread)?);
        world
            .finish(&[&swapped, &spent_again, &spread_again])
            .await?;
        world.lease(&leased).await?;
        world.maintain().await?;
        let (swap, spreads) = ([PolicyKind::Swap], [PolicyKind::Spread]);
        world
            .answer(&[
                (swapped.as_str(), run(swapped.as_str(), &swap)),
                (spent_again.as_str(), run(spent_again.as_str(), &swap)),
                (spread_again.as_str(), run(spread_again.as_str(), &spreads)),
            ])
            .await;
        world
            .answer(&[
                (spent.as_str(), run(spent.as_str(), &swap)),
                (spread.as_str(), run(spread.as_str(), &spreads)),
            ])
            .await;
        let stored = [
            world.on(PolicyKind::Swap, 1),
            world.on(PolicyKind::Spread, 1),
        ];
        world.turn_on(&stored).await?;

        let (evaluation, filing, counted) = world.pass(&stored).await?;
        assert_eq!(counts(&evaluation, PolicyKind::Swap), Some((2, 1, 1, 0)));
        assert_eq!(counts(&evaluation, PolicyKind::Spread), Some((1, 0, 1, 0)));
        assert_eq!(
            counted,
            PassCounts {
                revisions: 1,
                rerun_jobs: 1,
                marked_jobs: 2,
                reruns_recorded: 1,
                ..PassCounts::default()
            }
        );
        let filing = filing.context("a filing")?;
        let FilingOutcome::Advanced {
            plan_id,
            result: Ok(revised),
        } = &filing.outcome
        else {
            anyhow::bail!("the filing did not advance: {:?}", filing.outcome);
        };
        assert_eq!(
            (
                revised.rerun_job_count,
                revised.marked_job_count,
                revised.carried_job_count,
                revised.dropped_job_count
            ),
            (1, 2, 2, 0)
        );
        assert_eq!(
            world.current().await?.current_plan_id.as_ref(),
            Some(plan_id)
        );
        let filed = world.manifest(plan_id).await?;
        let minted = |of: &JobId| {
            filed
                .rerun_of
                .iter()
                .find(|(_, earlier)| *earlier == of)
                .map(|(job_id, _)| job_id.clone())
                .context("a job minted for it")
        };
        let (rerun, marked, spread_marked) = (
            minted(&swapped)?,
            minted(&spent_again)?,
            minted(&spread_again)?,
        );
        assert!(
            [&queued, &leased, &rerun, &marked, &spread_marked]
                .iter()
                .all(|job_id| filed.job_ids.contains(job_id))
        );
        assert!(
            [&swapped, &spent_again, &spread_again]
                .iter()
                .all(|job_id| !filed.job_ids.contains(job_id))
        );
        // The queued job waits under its id, and the leased one keeps its
        // device, each a job of the filed plan.
        let todo = &world.stores.todo;
        assert_eq!(
            todo.avail_for_jobs(std::slice::from_ref(&queued))
                .await?
                .len(),
            1
        );
        let leases = todo.leases_for_jobs(std::slice::from_ref(&leased)).await?;
        assert_eq!(
            leases
                .iter()
                .map(|lease| lease.client_id.as_str())
                .collect::<Vec<_>>(),
            ["ev1_a"]
        );
        let markers = futures::future::try_join_all(
            [&queued, &leased].map(|job_id| world.stores.plans.list_job_plan_markers(job_id)),
        )
        .await?;
        assert!(
            markers
                .iter()
                .all(|held| held.iter().any(|(holder, _)| holder == plan_id))
        );

        let group_id = &world.group.group_id;
        let notes = world.stores.plans.get_plan_notes(plan_id).await?;
        assert_eq!(
            notes.change,
            Some(RevisionChange::Rerun {
                answered: None,
                job_count: 1,
                marked_job_count: 2,
                group_id: Some(group_id.clone()),
                policies: vec![PolicyKind::Swap, PolicyKind::Spread],
            })
        );
        assert_eq!(
            notes.note,
            Some(format!(
                "Campaign group {group_id} policies swap, spread: re-run 1, marked 2 failed."
            ))
        );
        let submissions = &*world.stores.submissions;
        let reasons =
            futures::future::try_join_all([&marked, &spread_marked].map(|job_id| async move {
                let record = submissions
                    .find_job(job_id)
                    .await?
                    .context("the mark's record")?;
                let Submission::Failure(failure) = parse_stored_submission(&record.body)? else {
                    anyhow::bail!("the mark's record is no failure: {}", record.body);
                };
                anyhow::Ok(failure.wire.failure_reason.as_str().to_owned())
            }))
            .await?;
        assert_eq!(
            reasons,
            ["swap", "spread"].map(|kind| format!(
                "The {kind} policy of campaign group {group_id} marked this job failed: flagged \
                 on 2 runs in a row, 1 re-run spent."
            ))
        );

        let kept = world.kept().await?.context("the member's record")?;
        assert_eq!(
            (
                kept.policies.as_slice(),
                &world.decided().await?,
                &kept.triggered
            ),
            (
                stored.as_slice(),
                &BTreeSet::new(),
                &BTreeSet::from([PolicyKind::Swap, PolicyKind::Spread])
            )
        );
        let recorded = kept.filed.context("the filing's record")?;
        assert_eq!(
            (&recorded.plan_id, &recorded.queued, recorded.commit_unknown),
            (
                plan_id,
                &BTreeSet::from([rerun.clone(), marked, spread_marked]),
                false
            )
        );

        // The re-run waits for its rows; the mark is no re-run.
        assert_eq!(
            world.reruns().await?,
            [PolicyRerun {
                campaign_id: world.campaign.campaign_id.clone(),
                rerun_job_id: rerun,
                rerun_of: swapped,
                plan_id: plan_id.clone(),
                filed_at: filed.created_at.round(
                    jiff::TimestampRound::new()
                        .smallest(jiff::Unit::Microsecond)
                        .mode(jiff::RoundMode::Trunc)
                )?,
                before: 4096.0,
                threshold: Some(0.0),
                ..a_rerun(PolicyKind::Swap)?
            }]
        );
        Ok(())
    }

    /// After a filing, the campaign waits while a device has not taken its
    /// re-run, and nothing of its rows is read, though the re-run keeps its
    /// policy triggered. Once one has, the next round
    /// reads only the job answered since, and a round with nothing new reads
    /// nothing.
    #[tokio::test]
    async fn a_filing_waits_for_devices_then_only_new_answers_are_read() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let [swapped, kept, later, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        let stored = [world.on(PolicyKind::Swap, 3)];
        world.turn_on(&stored).await?;
        world.finish(&[swapped, kept]).await?;
        world.maintain().await?;
        world
            .answer(&[
                (swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap])),
                (kept.as_str(), run(kept.as_str(), &[])),
            ])
            .await;
        let (_, filing, _) = world.pass(&stored).await?;
        let filed = filing
            .and_then(|filing| filing.record?.filed)
            .context("the filing's record")?;
        let rerun = filed.queued.first().context("the filing's re-run")?;
        // A state with no policy triggered, as an older binary kept one.
        let filed_state = world.kept().await?.context("the filing's record")?;
        let unmarked = GroupPolicyState {
            triggered: BTreeSet::new(),
            ..filed_state
        };
        world.keep(&unmarked, &world.job_states().await?).await?;

        world.duckdb.reset_async().await;
        let (waiting, _, counted) = world.pass(&stored).await?;
        // It reads no rows while held, so its waiting re-run triggers swap.
        assert_eq!(
            world.kept().await?.map(|state| state.triggered),
            Some(BTreeSet::from([PolicyKind::Swap]))
        );
        assert_eq!(
            (
                waiting.state,
                waiting.hold,
                counted.members_waiting_for_devices
            ),
            (
                PolicyMemberState::WaitingForDevices,
                Some(Hold::Queued {
                    plan_id: filed.plan_id.clone(),
                    waiting: 1
                }),
                1
            )
        );

        world.lease(rerun).await?;
        world.finish(&[later]).await?;
        world.maintain().await?;
        let again = world
            .duckdb
            .mock_async(|when, then| {
                when.method(POST).body_includes(format!("'{kept}'"));
                then.status(500);
            })
            .await;
        let new = world
            .answer(&[(later.as_str(), run(later.as_str(), &[]))])
            .await;
        let (read, filing, _) = world.pass(&stored).await?;
        assert_eq!(read.state, PolicyMemberState::Evaluated);
        assert!(filing.is_none());
        again.assert_calls_async(0).await;
        new.assert_calls_async(1).await;
        assert_eq!(
            world.decided().await?,
            BTreeSet::from([kept.clone(), later.clone()])
        );

        // No query is answered now, so a round that read rows would read
        // unreadable.
        world.duckdb.reset_async().await;
        let (quiet, _, _) = world.pass(&stored).await?;
        assert_eq!(quiet.state, PolicyMemberState::Evaluated);
        Ok(())
    }

    /// A filing records each re-run waiting. The round that reads a re-run's
    /// rows judges it: cleared once swap no longer flags it, flagged while it
    /// does, and a flagged one re-run again records a row of the next attempt.
    /// A round that fails to write the outcomes neither files nor keeps
    /// anything, so the next round judges the same re-runs.
    #[tokio::test]
    async fn a_rerun_is_judged_on_its_rows_and_rerun_again_at_the_next_attempt()
    -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let [clears, stays, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        let stored = [world.on(PolicyKind::Swap, 3)];
        world.turn_on(&stored).await?;
        world.finish(&[clears, stays]).await?;
        world.maintain().await?;
        let swap = [PolicyKind::Swap];
        world
            .answer(&[
                (clears.as_str(), run(clears.as_str(), &swap)),
                (stays.as_str(), run(stays.as_str(), &swap)),
            ])
            .await;
        let (_, _, counted) = world.pass(&stored).await?;
        assert_eq!((counted.reruns_recorded, counted.reruns_judged), (2, 0));
        let waiting = world.reruns().await?;
        let minted = |of: &JobId| {
            waiting
                .iter()
                .find(|rerun| rerun.rerun_of == *of)
                .context("a re-run of it")
        };
        let (cleared, flagged) = (minted(clears)?, minted(stays)?);
        assert!(
            waiting
                .iter()
                .all(|rerun| (rerun.attempt, rerun.outcome) == (1, None)),
            "{waiting:?}"
        );

        let (cleared_job, flagged_job) = (&cleared.rerun_job_id, &flagged.rerun_job_id);
        world.finish(&[cleared_job, flagged_job]).await?;
        world.maintain().await?;
        world.duckdb.reset_async().await;
        world
            .answer(&[
                (cleared_job.as_str(), run(cleared_job.as_str(), &[])),
                (flagged_job.as_str(), run(flagged_job.as_str(), &swap)),
            ])
            .await;
        world
            .answer(&[(stays.as_str(), run(stays.as_str(), &swap))])
            .await;
        world.db.execute(REFUSE_OUTCOME_WRITE).await?;
        let (_, unfiled, counted) = world.pass(&stored).await?;
        assert_eq!(
            (
                unfiled.is_none(),
                counted.failed_items,
                world.plan_count().await?,
                &world.reruns().await?
            ),
            (true, 1, 2, &waiting)
        );
        world
            .db
            .execute("DROP TRIGGER refuse_outcome_write ON campaign_group_rerun_results")
            .await?;
        let (_, filing, counted) = world.pass(&stored).await?;
        assert!(
            matches!(
                filing.map(|filing| filing.outcome),
                Some(FilingOutcome::Advanced { result: Ok(_), .. })
            ),
            "the flagged re-run is re-run again"
        );
        assert_eq!((counted.reruns_recorded, counted.reruns_judged), (1, 2));
        let rows = world.reruns().await?;
        let [again, earlier @ ..] = rows.as_slice() else {
            anyhow::bail!("no rows");
        };
        assert_eq!(
            (&again.rerun_of, again.attempt, again.before, again.outcome),
            (flagged_job, 2, 4096.0, None)
        );
        let judged = |rerun: &PolicyRerun| {
            earlier
                .iter()
                .find(|row| row.rerun_job_id == rerun.rerun_job_id)
                .map(|row| (row.after, row.outcome, row.judged_at.is_some()))
        };
        assert_eq!(
            [judged(cleared), judged(flagged)],
            [
                Some((Some(0.0), Some(RerunOutcome::Cleared), true)),
                Some((Some(4096.0), Some(RerunOutcome::Flagged), true)),
            ]
        );
        Ok(())
    }

    /// The backfill records what a group's filing re-ran before the pass
    /// recorded it, as the pass records it, judged once the re-run's rows
    /// landed. It leaves out the filing's mark, and swap, which flags the
    /// re-run's earlier run too but has no re-run left. A second run records
    /// nothing.
    #[tokio::test]
    async fn the_backfill_records_a_filings_reruns_once() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let [spread, swapped, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        let stored = [
            world.on(PolicyKind::Swap, 0),
            world.on(PolicyKind::Spread, 1),
        ];
        world.turn_on(&stored).await?;
        world.finish(&[spread, swapped]).await?;
        world.maintain().await?;
        let both = [PolicyKind::Swap, PolicyKind::Spread];
        let before = [
            (spread.as_str(), run(spread.as_str(), &both)),
            (swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap])),
        ];
        world.answer(&before).await;
        let (evaluation, _, _) = world.pass(&stored).await?;
        assert_eq!(evaluation.marked.len(), 1);
        let [recorded] = <[PolicyRerun; 1]>::try_from(world.reruns().await?)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        assert_eq!(
            (recorded.kind, &recorded.rerun_of),
            (PolicyKind::Spread, spread)
        );

        // The re-run answers, and its row is lost, as for a filing before the
        // pass recorded re-runs.
        let rerun = recorded.rerun_job_id.clone();
        world.finish(&[&rerun]).await?;
        world.duckdb.reset_async().await;
        let rows = run(rerun.as_str(), &[]);
        world
            .answer(&[before[0].clone(), before[1].clone(), (rerun.as_str(), rows)])
            .await;
        world
            .db
            .execute("DELETE FROM campaign_group_rerun_results")
            .await?;
        let results = ResultsDb::new(
            reqwest::Client::new(),
            world.config().duckdb.context("the mock pipette-duckdb")?,
        )?;
        let group_ids = std::slice::from_ref(&world.group.group_id);
        let first = backfill(&world.stores, &results, group_ids).await?;
        assert_eq!(
            first,
            Backfilled {
                filings: 1,
                found: 1,
                recorded: 1,
            }
        );
        let [judged] = <[PolicyRerun; 1]>::try_from(world.reruns().await?)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        assert!(judged.judged_at.is_some());
        assert_eq!(
            PolicyRerun {
                judged_at: None,
                ..judged.clone()
            },
            PolicyRerun {
                after: Some(1.0),
                outcome: Some(RerunOutcome::Cleared),
                ..recorded
            }
        );

        let second = backfill(&world.stores, &results, group_ids).await?;
        assert_eq!(
            second,
            Backfilled {
                recorded: 0,
                ..first
            }
        );
        assert_eq!(world.reruns().await?, [judged]);
        Ok(())
    }

    /// The backfill gives no row for a group filing's re-run of an eval's
    /// cell, whatever the replaced run's rows, and one for its decode cell.
    #[tokio::test]
    async fn the_backfill_records_no_rerun_of_an_eval() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        world.enroll().await?;
        let stored = [world.on(PolicyKind::Swap, 3)];
        world.turn_on(&stored).await?;
        let earlier = PlanManifest {
            previous_plan_id: None,
            ..world.plan("plan-1", &[("job-a", None), ("job-b", None)])?
        };
        let filed = PlanManifest {
            previous_plan_id: Some(earlier.plan_id.clone()),
            benchmark_types: BTreeMap::from([
                (BenchmarkId::try_new("eval_math_500")?, BenchmarkType::Eval),
                (
                    BenchmarkId::try_new("decode_throughput_512_100")?,
                    BenchmarkType::DecodeThroughput,
                ),
            ]),
            ..world.plan(HEAD, &[("job-c", Some("job-a")), ("job-d", Some("job-b"))])?
        };
        world.put(&[earlier, filed]).await?;
        let plans = &*world.stores.plans;
        let change = RevisionChange::Rerun {
            answered: None,
            job_count: 2,
            marked_job_count: 0,
            group_id: Some(world.group.group_id.clone()),
            policies: vec![PolicyKind::Swap],
        };
        let notes = PlanNotes {
            note: None,
            change: Some(change),
        };
        anyhow::ensure!(
            plans
                .put_plan_notes(&PlanId::try_new(HEAD)?, &notes)
                .await?,
            "the filing's notes are kept"
        );
        world
            .source(&[
                ("job-c", "eval_math_500"),
                ("job-d", "decode_throughput_512_100"),
            ])
            .await?;
        let swap = [PolicyKind::Swap];
        let rows: Vec<Value> = [run("job-a", &swap), run("job-b", &swap)].concat();
        world
            .duckdb
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/api/query")
                    .body_includes("'job-b'");
                then.status(200)
                    .json_body(json!({ "columns": COLUMNS, "rows": rows }));
            })
            .await;
        let results = ResultsDb::new(
            reqwest::Client::new(),
            world.config().duckdb.context("the mock pipette-duckdb")?,
        )?;
        backfill(
            &world.stores,
            &results,
            std::slice::from_ref(&world.group.group_id),
        )
        .await?;
        let recorded: Vec<(PolicyKind, JobId, JobId, u32)> = world
            .reruns()
            .await?
            .into_iter()
            .map(|rerun| {
                (
                    rerun.kind,
                    rerun.rerun_job_id,
                    rerun.rerun_of,
                    rerun.attempt,
                )
            })
            .collect();
        assert_eq!(
            recorded,
            [(PolicyKind::Swap, job("job-d"), job("job-b"), 1)]
        );
        Ok(())
    }

    /// A waiting re-run that its check no longer flags is failed when a row of
    /// it failed: a swap or spread re-run that failed outright, and a failed
    /// one whose code that policy does not re-run. A failed one that fails
    /// again with a code it re-runs is flagged. One the head no longer holds
    /// is judged on its own rows, or dropped with neither rows nor a record;
    /// one the head holds without rows waits.
    #[tokio::test]
    async fn a_rerun_is_failed_by_a_failed_row_and_dropped_once_gone_without_rows()
    -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let jobs = [
            ("job-1", None),
            ("job-2", None),
            ("job-3", None),
            ("job-4", None),
        ];
        world.put(&[world.plan(HEAD, &jobs)?]).await?;
        world
            .answer(&[
                ("job-1", run("job-1", &[])),
                ("job-2", failed_run("job-2", "client", "runtime_error")),
                ("job-3", failed_run("job-3", "client", "attempt_timeout")),
                ("job-4", Vec::new()),
            ])
            .await;
        let left = world
            .answer(&[
                ("job-left", run("job-left", &[PolicyKind::Swap])),
                ("job-gone", Vec::new()),
            ])
            .await;
        let rerun = |job_id: &str, kind: PolicyKind, threshold: Option<f64>| {
            anyhow::Ok(PolicyRerun {
                campaign_id: world.campaign.campaign_id.clone(),
                rerun_job_id: job(job_id),
                rerun_of: job("job-0"),
                threshold,
                ..a_rerun(kind)?
            })
        };
        let waiting = [
            rerun("job-1", PolicyKind::Swap, Some(0.0))?,
            rerun("job-2", PolicyKind::Swap, Some(0.0))?,
            rerun("job-2", PolicyKind::Spread, Some(5.0))?,
            rerun("job-2", PolicyKind::Failed, None)?,
            rerun("job-3", PolicyKind::Failed, None)?,
            rerun("job-4", PolicyKind::Swap, Some(0.0))?,
            rerun("job-left", PolicyKind::Swap, Some(0.0))?,
            rerun("job-gone", PolicyKind::Swap, Some(0.0))?,
        ];
        let state = GroupPolicyState::default();
        let pass = Pass {
            state: &state,
            todo: &*world.stores.todo,
            submissions: &*world.stores.submissions,
            preview: false,
            table: None,
            catalog: None,
            waiting: &waiting,
            job_states: &[],
            bodies: None,
        };
        let duckdb = world.config().duckdb.context("the mock pipette-duckdb")?;
        let results = ResultsDb::new(reqwest::Client::new(), duckdb)?;
        let evaluation = evaluate(
            &*world.stores.plans,
            Some(&results),
            &world.group,
            &world.campaign,
            &[],
            pass,
        )
        .await?;
        left.assert_calls_async(1).await;
        assert!(
            evaluation
                .judged
                .iter()
                .all(|rerun| rerun.judged_at.is_some())
        );
        let judged: Vec<_> = evaluation
            .judged
            .iter()
            .map(|rerun| {
                (
                    rerun.rerun_job_id.as_str(),
                    rerun.kind,
                    rerun.after,
                    rerun.after_code.as_deref(),
                    rerun.outcome,
                )
            })
            .collect();
        let (swap, spread, failed) = (PolicyKind::Swap, PolicyKind::Spread, PolicyKind::Failed);
        assert_eq!(
            judged,
            [
                ("job-1", swap, Some(0.0), None, Some(RerunOutcome::Cleared)),
                ("job-2", swap, None, None, Some(RerunOutcome::Failed)),
                ("job-2", spread, None, None, Some(RerunOutcome::Failed)),
                (
                    "job-2",
                    failed,
                    Some(1.0),
                    Some("runtime_error"),
                    Some(RerunOutcome::Flagged)
                ),
                (
                    "job-3",
                    failed,
                    Some(0.0),
                    Some("attempt_timeout"),
                    Some(RerunOutcome::Failed)
                ),
                (
                    "job-left",
                    swap,
                    Some(4096.0),
                    None,
                    Some(RerunOutcome::Flagged)
                ),
                ("job-gone", swap, None, None, Some(RerunOutcome::Dropped)),
            ]
        );
        Ok(())
    }

    /// A re-run that a person re-ran has a recorded result, so it waits for
    /// its rows, by the pass and by the backfill, though the campaign's plan no
    /// longer holds it, and the pass judges it once they land.
    #[tokio::test]
    async fn a_rerun_a_person_reran_waits_for_its_rows() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        world.pass(&stored).await?;
        let [recorded] = <[PolicyRerun; 1]>::try_from(world.reruns().await?)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        let rerun = &recorded.rerun_job_id;
        world.finish(&[rerun]).await?;
        campaign_admin::rerun_jobs(
            &world.stores,
            settings(),
            &world.campaign.campaign_id,
            BTreeSet::from([rerun.clone()]),
            None,
        )
        .await?;
        world.answer(&[(rerun.as_str(), Vec::new())]).await;
        let (_, _, counted) = world.pass(&stored).await?;
        assert_eq!(counted.reruns_judged, 0);
        assert_eq!(world.reruns().await?, std::slice::from_ref(&recorded));

        let swapped = world.jobs.first().context("a job")?;
        let backfilled = world
            .backfilled(&[
                (swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap])),
                (rerun.as_str(), Vec::new()),
            ])
            .await?;
        assert_eq!(backfilled, std::slice::from_ref(&recorded));

        world.duckdb.reset_async().await;
        world
            .answer(&[(rerun.as_str(), run(rerun.as_str(), &[]))])
            .await;
        let (_, _, counted) = world.pass(&stored).await?;
        let [judged] = <[PolicyRerun; 1]>::try_from(world.reruns().await?)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        assert!(judged.judged_at.is_some());
        assert_eq!(
            (
                counted.reruns_judged,
                PolicyRerun {
                    judged_at: None,
                    ..judged
                }
            ),
            (
                1,
                PolicyRerun {
                    after: Some(0.0),
                    outcome: Some(RerunOutcome::Cleared),
                    ..recorded
                }
            )
        );
        Ok(())
    }

    /// A re-run that a person marked failed while it waited in the queue has
    /// no recorded result, so the pass and the backfill drop it once the
    /// campaign's plan no longer holds it.
    #[tokio::test]
    async fn a_rerun_the_campaign_no_longer_runs_is_dropped() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        world.pass(&stored).await?;
        let [recorded] = <[PolicyRerun; 1]>::try_from(world.reruns().await?)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        let rerun = &recorded.rerun_job_id;
        let plans = &*world.stores.plans;
        let revision = Head::read(plans, world.current().await?, None)
            .await?
            .rerun(
                plans,
                &*world.stores.submissions,
                BTreeSet::new(),
                BTreeMap::from([(rerun.clone(), NonEmptyTrimmedString::try_new("unwanted")?)]),
            )
            .await?;
        let catalog = crate::org_catalog::load_server_catalog(&*world.stores.catalog).await?;
        let ctx = IngestContext::with_settings(&world.stores, &catalog, settings());
        let membership = revision.membership(plans).await?;
        let change = Some(revision.change());
        let input = PlanInput::Text(revision.document());
        let report = plan_cli::ingest(&ctx, input, None, change, membership).await?;
        campaign_admin::complete_revision(
            &*world.stores.campaigns,
            plans,
            revision,
            &report.plan_id,
            &report.job_ids,
        )
        .await?;
        // The teardown of the plan the mark replaced takes the re-run out of
        // the queue.
        world.maintain().await?;
        let marked = world
            .manifest(&report.plan_id)
            .await?
            .rerun_of
            .into_iter()
            .find(|(_, of)| of == rerun)
            .map(|(job, _)| job)
            .context("the mark")?;
        world
            .answer(&[(marked.as_str(), mark(marked.as_str()))])
            .await;
        world.answer(&[(rerun.as_str(), Vec::new())]).await;
        let (_, _, counted) = world.pass(&stored).await?;
        let dropped = PolicyRerun {
            outcome: Some(RerunOutcome::Dropped),
            ..recorded.clone()
        };
        let [passed] = <[PolicyRerun; 1]>::try_from(world.reruns().await?)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        assert!(passed.judged_at.is_some());
        assert_eq!(
            (
                counted.reruns_judged,
                PolicyRerun {
                    judged_at: None,
                    ..passed
                }
            ),
            (1, dropped.clone())
        );

        let swapped = world.jobs.first().context("a job")?;
        let backfilled = world
            .backfilled(&[
                (swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap])),
                (rerun.as_str(), Vec::new()),
            ])
            .await?;
        let [backfilled] = <[PolicyRerun; 1]>::try_from(backfilled)
            .map_err(|rows| anyhow::anyhow!("one re-run, not {rows:?}"))?;
        assert!(backfilled.judged_at.is_some());
        assert_eq!(
            PolicyRerun {
                judged_at: None,
                ..backfilled
            },
            dropped
        );
        Ok(())
    }

    /// The backfill counts a row's `attempt` as the pass does, over the runs
    /// in a row that its policy flags: where spread flags a chain twice and
    /// then only failed flags it, failed's re-run is at attempt 1. It records
    /// the rows the pass recorded, failed's with its policy's codes.
    #[tokio::test]
    async fn the_backfill_counts_the_runs_in_a_row_each_policy_flags() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let failed = CampaignGroupPolicy {
            settings: PolicySettings {
                codes: Some([(FailureCode::RuntimeError, 1)].into()),
                ..PolicySettings::default()
            },
            ..world.on(PolicyKind::Failed, 0)
        };
        let stored = [world.on(PolicyKind::Spread, 2), failed];
        world.turn_on(&stored).await?;
        let spreads = |job: &JobId| run(job.as_str(), &[PolicyKind::Spread]);
        let fails = |job: &JobId| failed_run(job.as_str(), "client", "runtime_error");
        let first = world.jobs.first().context("a job")?;
        let second = world.reran(&stored, first, spreads(first)).await?;
        let third = world.reran(&stored, &second, spreads(&second)).await?;
        let fourth = world.reran(&stored, &third, fails(&third)).await?;
        let passed = world.reruns().await?;
        let codes: Vec<_> = passed.iter().map(|rerun| rerun.codes.clone()).collect();
        assert_eq!(codes, [Some(vec![FailureCode::RuntimeError]), None, None]);
        let backfilled = world
            .backfilled(&[
                (first.as_str(), spreads(first)),
                (second.as_str(), spreads(&second)),
                (third.as_str(), fails(&third)),
                (fourth.as_str(), Vec::new()),
            ])
            .await?;
        let attempts: Vec<(PolicyKind, u32)> = backfilled
            .iter()
            .map(|rerun| (rerun.kind, rerun.attempt))
            .collect();
        assert_eq!(
            attempts,
            [
                (PolicyKind::Failed, 1),
                (PolicyKind::Spread, 2),
                (PolicyKind::Spread, 1)
            ]
        );
        let unjudged = |rows: Vec<PolicyRerun>| -> Vec<PolicyRerun> {
            rows.into_iter()
                .map(|rerun| PolicyRerun {
                    judged_at: None,
                    ..rerun
                })
                .collect()
        };
        assert_eq!(unjudged(backfilled), unjudged(passed));
        Ok(())
    }

    /// The backfill gives a failed row the re-runs of the replaced run's code,
    /// as the policy is stored now: once device_not_ready drops to 1 re-run
    /// while runtime_error keeps 2, a runtime_error run's re-run at attempt 1
    /// is recorded and a device_not_ready run's at attempt 2 is not.
    #[tokio::test]
    async fn the_backfill_gives_each_run_the_reruns_of_its_code() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let failed = |device_not_ready| CampaignGroupPolicy {
            settings: PolicySettings {
                codes: Some(
                    [
                        (FailureCode::DeviceNotReady, device_not_ready),
                        (FailureCode::RuntimeError, 2),
                    ]
                    .into(),
                ),
                ..PolicySettings::default()
            },
            ..world.on(PolicyKind::Failed, 0)
        };
        let filed = failed(2);
        world.turn_on(std::slice::from_ref(&filed)).await?;
        let fails = |job: &JobId, code| failed_run(job.as_str(), "client", code);
        let first = world.jobs.first().context("a job")?;
        let stored = std::slice::from_ref(&filed);
        let second = world
            .reran(stored, first, fails(first, "runtime_error"))
            .await?;
        let third = world
            .reran(stored, &second, fails(&second, "device_not_ready"))
            .await?;
        assert!(world.groups()?.replace_policy(&filed, &failed(1)).await?);
        let backfilled = world
            .backfilled(&[
                (first.as_str(), fails(first, "runtime_error")),
                (second.as_str(), fails(&second, "device_not_ready")),
                (third.as_str(), Vec::new()),
            ])
            .await?;
        let rows: Vec<(u32, &JobId)> = backfilled
            .iter()
            .map(|rerun| (rerun.attempt, &rerun.rerun_of))
            .collect();
        assert_eq!(rows, [(1, first)]);
        Ok(())
    }

    /// How the member row comes to lose a filing that advanced.
    #[derive(Debug, Clone, Copy)]
    enum Lost {
        /// The campaign left the group and joined it again: a new row.
        Readded,
        /// The write after the filing failed: the row as it was before.
        Unwritten,
    }

    /// A head that a group's filing put there holds its campaign while the
    /// filing's re-run waits for a device, though the member row lost it.
    #[rstest::rstest]
    #[case::readded(Lost::Readded)]
    #[case::unwritten(Lost::Unwritten)]
    #[tokio::test]
    async fn a_group_filing_holds_its_campaign_without_its_record(
        #[case] lost: Lost,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let [swapped, kept, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        let stored = [world.on(PolicyKind::Swap, 3)];
        world.turn_on(&stored).await?;
        world.finish(&[kept]).await?;
        world.maintain().await?;
        world
            .answer(&[(kept.as_str(), run(kept.as_str(), &[]))])
            .await;
        let (first, nothing, _) = world.pass(&stored).await?;
        assert!(first.state == PolicyMemberState::Evaluated && nothing.is_none());

        world.finish(&[swapped]).await?;
        world.maintain().await?;
        world
            .answer(&[(swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap]))])
            .await;
        if let Lost::Unwritten = lost {
            world.db.execute(REFUSE_STATE_WRITE).await?;
        }
        let (_, filing, counted) = world.pass(&stored).await?;
        let outcome = filing.map(|filing| filing.outcome);
        assert!(
            matches!(outcome, Some(FilingOutcome::Advanced { result: Ok(_), .. })),
            "{outcome:?}"
        );
        match lost {
            Lost::Readded => {
                let groups = world.groups()?;
                let campaign_id = &world.campaign.campaign_id;
                groups
                    .remove_member(&world.group.group_id, campaign_id)
                    .await?;
                groups
                    .add_members(&world.group.group_id, std::slice::from_ref(campaign_id))
                    .await?;
                assert_eq!((counted.failed_items, world.kept().await?), (0, None));
            }
            Lost::Unwritten => {
                world
                    .db
                    .execute("DROP TRIGGER refuse_state_write ON campaign_group_members")
                    .await?;
                let row = world.kept().await?.context("the first round's record")?;
                assert_eq!((counted.failed_items, row.filed), (1, None));
            }
        }

        let head = world.current().await?.current_plan_id.context("a head")?;
        let (held, _, _) = world.pass(&stored).await?;
        assert_eq!(
            (held.state, held.hold),
            (
                PolicyMemberState::WaitingForDevices,
                Some(Hold::Queued {
                    plan_id: head,
                    waiting: 1
                })
            )
        );
        Ok(())
    }

    /// A head someone stops between the evaluation and the filing is not filed
    /// into, and nothing is filed.
    #[tokio::test]
    async fn a_head_stopped_before_the_filing_is_not_filed_into() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (stored, campaign, evaluation) = world.evaluated().await?;
        let head = campaign.current_plan_id.clone().context("a head")?;
        world.stores.plans.write_cancel_marker(&head).await?;
        let filing = world.file(&campaign, &stored, &evaluation).await?;
        assert!(
            matches!(filing.outcome, FilingOutcome::Stopped),
            "{:?}",
            filing.outcome
        );
        assert_eq!(world.current().await?.current_plan_id, Some(head));
        let filed = filing.record.and_then(|record| record.filed);
        assert_eq!((world.plan_count().await?, filed), (1, None));
        Ok(())
    }

    /// A head someone stops while the ingest runs is not filed into: the
    /// campaign keeps it, and the filed plan is recorded and cancelled, by the
    /// next round if that cancel fails. The next maintenance then retires the
    /// jobs the filing carried, as the stop meant, rather than release them to
    /// the filed plan.
    #[rstest::rstest]
    #[case::cancelled(true)]
    #[case::cancel_failed(false)]
    #[tokio::test]
    async fn a_head_stopped_during_the_ingest_is_not_filed_into(
        #[case] cancel_lands: bool,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        let head = world.current().await?.current_plan_id.context("a head")?;
        world.db.execute(STOP_THE_HEAD).await?;
        if !cancel_lands {
            world.db.execute(REFUSE_CANCEL).await?;
        }
        let (_, filing, counted) = world.pass(&stored).await?;
        let filing = filing.context("a filing")?;
        let FilingOutcome::StoppedDuringFiling { plan_id, cancelled } = &filing.outcome else {
            anyhow::bail!("the filing was not stopped: {:?}", filing.outcome);
        };
        assert_eq!(*cancelled, cancel_lands);
        assert_eq!(
            counted,
            PassCounts {
                filings_stopped: 1,
                failed_items: usize::from(!cancel_lands),
                ..PassCounts::default()
            }
        );
        assert_eq!(
            world.audited().await?,
            [(AuditOutcome::Failed, world.targets(&[&head, plan_id]))]
        );
        assert_eq!(world.current().await?.current_plan_id, Some(head.clone()));
        assert!(world.stores.plans.has_cancel_marker(&head).await?);
        let filed = world.kept().await?.and_then(|record| record.filed);
        assert_eq!(filed.map(|filed| filed.plan_id).as_ref(), Some(plan_id));

        world
            .db
            .execute(
                "DROP TRIGGER stop_the_head ON plans;
                 DROP TRIGGER IF EXISTS refuse_cancel ON cancelled_plans;",
            )
            .await?;
        let (next, again, counted) = world.pass(&stored).await?;
        assert!(again.is_none());
        if cancel_lands {
            assert_eq!(
                (next.state, counted),
                (PolicyMemberState::Cancelled, PassCounts::default())
            );
        } else {
            assert_eq!(
                (next.state, counted),
                (
                    PolicyMemberState::FilingNotAdvanced,
                    PassCounts {
                        filings_not_advanced: 1,
                        cancel_markers_written: 1,
                        ..PassCounts::default()
                    }
                )
            );
        }
        assert!(world.stores.plans.has_cancel_marker(plan_id).await?);
        let carried = world.jobs.get(1..).context("five jobs")?;
        let todo = &world.stores.todo;
        assert_eq!(todo.avail_for_jobs(carried).await?.len(), carried.len());
        world.maintain().await?;
        assert!(todo.avail_for_jobs(carried).await?.is_empty());
        Ok(())
    }

    /// A person's revision lands while the pass's ingest runs: the campaign
    /// keeps it, and the pass's plan is cancelled. A cancel that fails is
    /// written by the next round, which otherwise evaluates the person's head.
    #[rstest::rstest]
    #[case::cancelled(true)]
    #[case::cancel_failed(false)]
    #[tokio::test]
    async fn a_revision_landing_during_the_ingest_overtakes_the_filing(
        #[case] cancel_lands: bool,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        let [swapped, person, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        world.finish(&[person]).await?;
        let campaign = world.current().await?;
        let head = campaign.current_plan_id.clone().context("a head")?;
        let theirs = world.stage_rerun(&campaign, person).await?;
        world.db.execute(A_PERSON_REVISES).await?;
        if !cancel_lands {
            world.db.execute(REFUSE_CANCEL).await?;
        }
        let (_, filing, counted) = world.pass(&stored).await?;
        let filing = filing.context("a filing")?;
        let FilingOutcome::NotAdvanced {
            plan_id,
            error: CampaignAdminError::Overtaken { cancelled, .. },
        } = &filing.outcome
        else {
            anyhow::bail!("the filing was not overtaken: {:?}", filing.outcome);
        };
        assert_eq!(*cancelled, cancel_lands);
        assert_eq!(
            counted,
            PassCounts {
                filings_not_advanced: 1,
                failed_items: usize::from(!cancel_lands),
                ..PassCounts::default()
            }
        );
        assert_eq!(
            world.audited().await?,
            [(AuditOutcome::Failed, world.targets(&[&head, plan_id]))]
        );
        assert_eq!(
            world.current().await?.current_plan_id.as_ref(),
            Some(&theirs)
        );
        assert_eq!(
            world.stores.plans.has_cancel_marker(plan_id).await?,
            cancel_lands
        );
        world
            .db
            .execute(
                "DROP TRIGGER a_person_revises ON plans;
                 DROP TRIGGER IF EXISTS refuse_cancel ON cancelled_plans;",
            )
            .await?;
        world.maintain().await?;
        let (next, _, counted) = world.pass(&stored).await?;
        if cancel_lands {
            assert_eq!(
                (next.state, next.plan_id, next.rerun_job_ids),
                (
                    PolicyMemberState::Evaluated,
                    Some(theirs),
                    BTreeSet::from([swapped.clone()])
                )
            );
        } else {
            assert_eq!(
                (next.state, next.hold, counted),
                (
                    PolicyMemberState::FilingNotAdvanced,
                    Some(Hold::NotAdvanced {
                        plan_id: plan_id.clone(),
                        commit_unknown: false
                    }),
                    PassCounts {
                        filings_not_advanced: 1,
                        cancel_markers_written: 1,
                        ..PassCounts::default()
                    }
                )
            );
            assert!(world.stores.plans.has_cancel_marker(plan_id).await?);
        }
        Ok(())
    }

    /// A Turn off between the evaluation and the filing stops the filing.
    #[tokio::test]
    async fn a_policy_turned_off_before_the_filing_stops_it() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let ([on], campaign, evaluation) = world.evaluated().await?;
        let off = CampaignGroupPolicy {
            enabled: false,
            ..on.clone()
        };
        assert!(world.groups()?.replace_policy(&on, &off).await?);
        let filing = world
            .file(&campaign, std::slice::from_ref(&on), &evaluation)
            .await?;
        assert!(
            matches!(filing.outcome, FilingOutcome::PoliciesChanged),
            "{:?}",
            filing.outcome
        );
        assert_eq!(world.current().await?, campaign);
        assert_eq!(world.plan_count().await?, 1);
        Ok(())
    }

    /// Turning model_params on between the evaluation and the filing changes
    /// no filing, so it goes ahead.
    #[tokio::test]
    async fn model_params_turned_on_before_the_filing_does_not_stop_it() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let ([on], campaign, evaluation) = world.evaluated().await?;
        world
            .turn_on(&[world.on(PolicyKind::ModelParams, 0)])
            .await?;
        let filing = world
            .file(&campaign, std::slice::from_ref(&on), &evaluation)
            .await?;
        assert!(
            matches!(filing.outcome, FilingOutcome::Advanced { .. }),
            "{:?}",
            filing.outcome
        );
        Ok(())
    }

    /// A campaign that leaves the group between the evaluation and the filing
    /// is not filed into.
    #[tokio::test]
    async fn a_member_removed_before_the_filing_is_not_filed_into() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (stored, campaign, evaluation) = world.evaluated().await?;
        world
            .groups()?
            .remove_member(&world.group.group_id, &campaign.campaign_id)
            .await?;
        let filing = world.file(&campaign, &stored, &evaluation).await?;
        assert!(
            matches!(filing.outcome, FilingOutcome::LeftGroup),
            "{:?}",
            filing.outcome
        );
        assert_eq!(world.current().await?, campaign);
        assert_eq!(world.plan_count().await?, 1);
        Ok(())
    }

    /// How the campaign moves between the evaluation and the filing.
    #[derive(Debug, Clone, Copy)]
    enum Moved {
        /// Onto another plan.
        Head,
        /// To another owner, keeping its group.
        Owner,
    }

    /// A campaign that moves between the evaluation and the filing is
    /// contended, and nothing is filed. One handed to another owner reads so
    /// in the next round, which keeps nothing.
    #[rstest::rstest]
    #[case::head(Moved::Head)]
    #[case::owner(Moved::Owner)]
    #[tokio::test]
    async fn a_campaign_that_moves_before_the_filing_is_contended(
        #[case] moved: Moved,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (stored, campaign, evaluation) = world.evaluated().await?;
        let elsewhere = match moved {
            Moved::Head => {
                let other = PlanId::try_new("plan-other")?;
                world
                    .stores
                    .plans
                    .put_plan(&PlanManifest {
                        campaign_id: Some(campaign.campaign_id.clone()),
                        ..sample_manifest(other.clone(), PlanStatus::Active, false)?
                    })
                    .await?;
                Campaign {
                    current_plan_id: Some(other),
                    ..campaign.clone()
                }
            }
            Moved::Owner => Campaign {
                owner: CampaignOwner::User {
                    user_id: UserId::try_new("user_other")?,
                },
                ..campaign.clone()
            },
        };
        world.stores.campaigns.put_campaign(&elsewhere).await?;
        let plans = world.plan_count().await?;
        let filing = world.file(&campaign, &stored, &evaluation).await?;
        assert!(
            matches!(
                &filing.outcome,
                FilingOutcome::Skipped(CampaignAdminError::Contended(campaign_id))
                    if *campaign_id == campaign.campaign_id
            ),
            "{:?}",
            filing.outcome
        );
        assert_eq!(world.plan_count().await?, plans);
        assert_eq!(world.current().await?, elsewhere);
        match moved {
            Moved::Head => {}
            Moved::Owner => {
                let next = world
                    .round(&elsewhere, &stored, Some(&GroupPolicyState::default()))
                    .await?;
                assert_eq!(
                    (next.state, next.record),
                    (PolicyMemberState::OtherOwner, None)
                );
            }
        }
        Ok(())
    }

    /// A head write that fails, and whose read back shows the campaign on its
    /// old head, leaves the filing `CommitUnknown`. The record keeps the plan
    /// as such, and the next round holds the campaign, fails again naming the
    /// plan, and leaves it running for an operator, also once someone stopped
    /// the head.
    #[tokio::test]
    async fn a_filing_whose_head_write_is_unknown_holds_its_campaign() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        let head = world.current().await?.current_plan_id.context("a head")?;
        world.db.execute(REFUSE_HEAD_WRITE).await?;
        let (_, filing, counted) = world.pass(&stored).await?;
        let filing = filing.context("a filing")?;
        let FilingOutcome::NotAdvanced {
            plan_id,
            error: CampaignAdminError::CommitUnknown { .. },
        } = &filing.outcome
        else {
            anyhow::bail!("the filing's head write is known: {:?}", filing.outcome);
        };
        let unknown = PassCounts {
            filings_not_advanced: 1,
            failed_items: 1,
            ..PassCounts::default()
        };
        assert_eq!(counted, unknown);
        // The campaign may never run the filing's re-run.
        assert!(world.reruns().await?.is_empty());
        assert_eq!(
            world.audited().await?,
            [(AuditOutcome::Failed, world.targets(&[&head, plan_id]))]
        );
        let record = world.kept().await?.context("the record")?;
        assert!(
            record
                .filed
                .as_ref()
                .is_some_and(|filed| filed.plan_id == *plan_id && filed.commit_unknown)
        );
        world.stores.plans.write_cancel_marker(&head).await?;
        let (next, again, counted) = world.pass(&stored).await?;
        assert!(again.is_none());
        assert_eq!(
            (next.state, next.hold, counted),
            (
                PolicyMemberState::FilingNotAdvanced,
                Some(Hold::NotAdvanced {
                    plan_id: plan_id.clone(),
                    commit_unknown: true
                }),
                unknown
            )
        );
        assert!(!world.stores.plans.has_cancel_marker(plan_id).await?);
        Ok(())
    }

    /// What does not read once the ingest finished, before the campaign moves.
    #[derive(Debug, Clone, Copy)]
    enum Unread {
        /// The head, for the check that no one stopped it.
        Head,
        /// The campaign, for the advance.
        Campaign,
    }

    /// A filing that did not advance, and that no head write can still move
    /// the campaign onto, is cancelled at once and recorded: its jobs run
    /// nowhere, even if the member's state then fails to write.
    #[rstest::rstest]
    #[case::head(Unread::Head)]
    #[case::campaign(Unread::Campaign)]
    #[tokio::test]
    async fn a_filing_that_cannot_advance_is_cancelled_at_once(
        #[case] unread: Unread,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        world
            .db
            .execute(match unread {
                Unread::Head => UNREADABLE_HEAD,
                Unread::Campaign => UNREADABLE_CAMPAIGN,
            })
            .await?;
        let (_, filing, counted) = world.pass(&stored).await?;
        let filing = filing.context("a filing")?;
        let FilingOutcome::NotAdvanced {
            plan_id,
            error: CampaignAdminError::NotAdvanced { .. },
        } = &filing.outcome
        else {
            anyhow::bail!("the filing's outcome: {:?}", filing.outcome);
        };
        assert!(world.stores.plans.has_cancel_marker(plan_id).await?);
        assert_eq!(
            counted,
            PassCounts {
                filings_not_advanced: 1,
                failed_items: 1,
                ..PassCounts::default()
            }
        );
        let filed = world.kept().await?.and_then(|record| record.filed);
        assert_eq!(filed.map(|filed| filed.plan_id).as_ref(), Some(plan_id));
        Ok(())
    }

    /// Why the head's document cannot be filed again.
    #[derive(Debug, Clone, Copy)]
    enum Unfileable {
        /// It sets a deadline that has passed, which the ingest refuses.
        PastDeadline,
        /// It no longer parses, which the ingest's expansion refuses.
        Unparsable,
        /// It is gone, so nothing reaches the ingest.
        Gone,
    }

    /// A head whose document cannot be filed again is refused once, a failed
    /// item with an audit event once it reached the ingest, and the round
    /// after reads nothing of it and fails nothing.
    #[rstest::rstest]
    #[case::past_deadline(Unfileable::PastDeadline)]
    #[case::unparsable(Unfileable::Unparsable)]
    #[case::gone(Unfileable::Gone)]
    #[tokio::test]
    async fn a_head_that_cannot_be_filed_again_is_refused_once(
        #[case] why: Unfileable,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        let head = world.current().await?.current_plan_id.context("a head")?;
        world
            .db
            .execute(match why {
                Unfileable::PastDeadline => PASSED_DEADLINE,
                Unfileable::Unparsable => UNPARSABLE,
                Unfileable::Gone => "DELETE FROM plan_sources",
            })
            .await?;
        // The manifest names the digest of its document as edited.
        if let Some(source) = world.stores.plans.get_plan_source(&head).await? {
            let manifest = world.manifest(&head).await?;
            world
                .stores
                .plans
                .put_plan(&PlanManifest {
                    source_sha256: Some(source.sha256().clone()),
                    ..manifest
                })
                .await?;
        }
        let (_, filing, counted) = world.pass(&stored).await?;
        let outcome = filing.context("a filing")?.outcome;
        match why {
            Unfileable::PastDeadline => assert!(
                matches!(
                    &outcome,
                    FilingOutcome::Rejected(error) if matches!(
                        error.downcast_ref::<IngestError>(),
                        Some(IngestError::Rejected(RejectionError::Job {
                            kind: JobRejectionError::PastExpiresAt { .. },
                            ..
                        }))
                    )
                ),
                "{outcome:?}"
            ),
            Unfileable::Unparsable => assert!(
                matches!(
                    &outcome,
                    FilingOutcome::Rejected(error)
                        if matches!(error.downcast_ref::<DocumentError>(), Some(DocumentError::Parse(_)))
                ),
                "{outcome:?}"
            ),
            Unfileable::Gone => assert!(
                matches!(
                    &outcome,
                    FilingOutcome::Refused(CampaignAdminError::NoDocument(plan_id))
                        if *plan_id == head
                ),
                "{outcome:?}"
            ),
        }
        assert_eq!(
            counted,
            PassCounts {
                refused_heads: 1,
                failed_items: 1,
                ..PassCounts::default()
            }
        );
        let audited = match why {
            Unfileable::PastDeadline | Unfileable::Unparsable => {
                vec![(AuditOutcome::Failed, world.targets(&[&head]))]
            }
            Unfileable::Gone => Vec::new(),
        };
        assert_eq!(world.audited().await?, audited);
        let refused = world
            .kept()
            .await?
            .and_then(|record| record.refusal)
            .context("the refusal's record")?;
        assert_eq!(
            (&refused.plan_id, refused.build.as_str()),
            (&head, BUILD_VERSION)
        );

        world.duckdb.reset_async().await;
        let (next, again, counted) = world.pass(&stored).await?;
        assert!(again.is_none());
        assert_eq!(
            (next.state, next.hold, counted),
            (
                PolicyMemberState::Refused,
                Some(Hold::Refused(refused.reason)),
                PassCounts {
                    refused_heads: 1,
                    ..PassCounts::default()
                }
            )
        );

        // model_params files nothing, so turning it on keeps the refusal.
        let params = world.on(PolicyKind::ModelParams, 0);
        world.turn_on(std::slice::from_ref(&params)).await?;
        let [swap] = stored;
        let (next, again, counted) = world.pass(&[swap, params]).await?;
        assert!(again.is_none());
        assert_eq!(
            (next.state, counted),
            (
                PolicyMemberState::Refused,
                PassCounts {
                    refused_heads: 1,
                    ..PassCounts::default()
                }
            )
        );
        Ok(())
    }

    /// A refusal another build recorded does not hold its head: the next run files it.
    #[tokio::test]
    async fn a_new_release_tries_a_refused_head_once_more() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = world.one_swapped().await?;
        let head = world.current().await?.current_plan_id.context("a head")?;
        let earlier = GroupPolicyState {
            policies: stored.to_vec(),
            refusal: Some(Refusal {
                plan_id: head,
                reason: "the deadline has passed".to_owned(),
                build: format!("{BUILD_VERSION}-earlier"),
            }),
            ..GroupPolicyState::default()
        };
        let written = world
            .groups()?
            .set_policy_state(
                &world.group.group_id,
                &world.campaign.campaign_id,
                &earlier,
                &JobStateChanges::default(),
            )
            .await?;
        anyhow::ensure!(written, "the member's state was not written");

        let (_, filing, _) = world.pass(&stored).await?;
        let outcome = filing.map(|filing| filing.outcome);
        assert!(
            matches!(outcome, Some(FilingOutcome::Advanced { result: Ok(_), .. })),
            "{outcome:?}"
        );
        let kept = world.kept().await?.context("the run's record")?;
        assert_eq!(kept.refusal, None);
        Ok(())
    }

    /// One round through [`apply`] files the member's swapped job, and the
    /// filing's audit event, under the host, names the group and the policy
    /// and is in the log of the owner's organization. The plan index backend
    /// is probed once, for the filing.
    #[tokio::test]
    async fn a_round_files_a_revision_and_its_audit_event() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (org_id, owner) = (OrgId::mint(), UserId::try_new("user_nightly")?);
        world
            .stores
            .orgs
            .set_membership(&owner, Some(org_id.clone()))
            .await?;
        let turned_on_by = UserId::try_new("user_admin")?;
        world
            .swapped(CampaignGroupPolicy {
                enabled_by: Some(turned_on_by.clone()),
                ..world.on(PolicyKind::Swap, 3)
            })
            .await?;
        let head = world.current().await?.current_plan_id.context("a head")?;

        let probes = AtomicUsize::new(0);
        apply(&world.config(), &world.stores, Instant::now(), || {
            probes.fetch_add(1, Ordering::Relaxed);
            probed()
        })
        .await?;
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        let filed = world.current().await?.current_plan_id.context("a head")?;
        assert_ne!(filed, head);
        let kept = world.kept().await?.and_then(|record| record.filed);
        assert_eq!(kept.map(|kept| kept.plan_id).as_ref(), Some(&filed));

        let events = world
            .stores
            .audit
            .as_deref()
            .context("an audit log")?
            .list_events(
                &AuditFilter::default(),
                None,
                NonZeroUsize::MIN.saturating_add(9),
            )
            .await?
            .items;
        let [event] = events.as_slice() else {
            anyhow::bail!("{} audit events", events.len());
        };
        assert!(
            matches!(event.actor, AuditActor::Host { .. }),
            "{:?}",
            event.actor
        );
        assert_eq!(
            (&event.action, &event.outcome),
            (
                &AuditAction::GroupPolicy {
                    group_id: world.group.group_id.clone(),
                    kinds: vec![PolicyKind::Swap],
                    rerun_job_count: 1,
                    marked_job_count: 0,
                },
                &AuditOutcome::Succeeded
            )
        );
        assert_eq!(
            event.targets,
            BTreeSet::from([
                AuditTarget::Campaign {
                    campaign_id: world.campaign.campaign_id.clone()
                },
                AuditTarget::CampaignGroup {
                    group_id: world.group.group_id.clone()
                },
                AuditTarget::Plan { plan_id: head },
                AuditTarget::Plan { plan_id: filed },
                AuditTarget::User { user_id: owner },
                AuditTarget::User {
                    user_id: turned_on_by
                },
            ])
        );
        assert_eq!(event.org_ids, BTreeSet::from([org_id]));
        Ok(())
    }

    /// A round started more than a minute into its run reads no member, and
    /// counts each as not reached.
    #[tokio::test]
    async fn a_round_past_its_minute_reads_no_member() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        world
            .join("S26 NPU", &DOCUMENT.replace("4bit", "8bit"))
            .await?;
        world.one_swapped().await?;
        let started = Instant::now()
            .checked_sub(Duration::from_secs(61))
            .context("a minute ago")?;
        let counts = round(
            &world.config(),
            &world.stores,
            world.groups()?,
            started,
            &probed,
        )
        .await?;
        assert_eq!(
            (
                counts.members,
                counts.members_read,
                counts.members_not_reached,
                counts.revisions
            ),
            (2, 0, 2, 0)
        );
        assert_eq!(world.kept().await?, None);
        Ok(())
    }

    /// A member that files goes behind the others in the next round: of two
    /// members with something to file, the one with no filing yet is taken
    /// first, though the other comes first by campaign id.
    #[tokio::test]
    async fn a_member_that_filed_goes_behind_the_others() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (other, other_jobs) = world
            .join("S26 NPU", &DOCUMENT.replace("4bit", "8bit"))
            .await?;
        assert!(world.campaign.campaign_id < other.campaign_id);
        world.one_swapped().await?;
        let (config, groups) = (world.config(), world.groups()?);
        let first = round(&config, &world.stores, groups, Instant::now(), &probed).await?;
        let filed_first = world
            .kept()
            .await?
            .and_then(|record| record.filed)
            .context("the first round's filing")?;
        let rerun = filed_first.queued.first().context("the filing's re-run")?;

        world.lease(rerun).await?;
        let [_, mine, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        let theirs = other_jobs.first().context("a job")?;
        world.finish(&[mine, theirs]).await?;
        world.maintain().await?;
        let swap = [PolicyKind::Swap];
        world
            .answer(&[(mine.as_str(), run(mine.as_str(), &swap))])
            .await;
        world
            .answer(&[(theirs.as_str(), run(theirs.as_str(), &swap))])
            .await;
        let second = round(&config, &world.stores, groups, Instant::now(), &probed).await?;
        let filed_at = |record: Option<GroupPolicyState>| {
            record
                .and_then(|record| record.filed)
                .map(|filed| filed.filed_at)
        };
        let mine_at = filed_at(world.kept().await?).context("the campaign's filing")?;
        let theirs_at =
            filed_at(world.kept_of(&other.campaign_id).await?).context("the other's filing")?;
        assert_eq!(
            [&first, &second].map(|counts| (counts.members_read, counts.revisions)),
            [(2, 1), (2, 2)]
        );
        assert!(filed_first.filed_at < theirs_at && theirs_at < mine_at);
        Ok(())
    }

    /// The pass fails no run of a deployment without `[duckdb]` until a
    /// policy is on, and then fails each without reading a member.
    #[tokio::test]
    async fn a_policy_on_without_duckdb_fails_the_run() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let config = Config {
            duckdb: None,
            ..world.config()
        };
        apply(&config, &world.stores, Instant::now(), probed).await?;
        world.one_swapped().await?;
        let ran = apply(&config, &world.stores, Instant::now(), probed).await;
        let error = format!("{:#}", ran.err().context("the run passed")?);
        assert!(error.contains("1 policy pass item(s) failed"), "{error}");
        assert_eq!(world.kept().await?, None);
        Ok(())
    }

    /// A member whose campaign or kept state does not read is skipped this
    /// round, a failed item, and the round goes on to the next one.
    #[rstest::rstest]
    #[case::campaign(
        "UPDATE campaigns SET body = 'not a campaign' WHERE body::jsonb ->> 'name' = 'S26 CPU'"
    )]
    #[case::state(
        "UPDATE campaign_group_members m SET policy_state = 'not a state' FROM campaigns c
         WHERE c.campaign_id = m.campaign_id AND c.body::jsonb ->> 'name' = 'S26 CPU'"
    )]
    #[tokio::test]
    async fn a_member_that_does_not_read_is_skipped(
        #[case] unreadable: &'static str,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (other, _) = world
            .join("S26 NPU", &DOCUMENT.replace("4bit", "8bit"))
            .await?;
        world.turn_on(&[world.on(PolicyKind::Swap, 3)]).await?;
        world.db.execute(unreadable).await?;
        let counts = round(
            &world.config(),
            &world.stores,
            world.groups()?,
            Instant::now(),
            &probed,
        )
        .await?;
        assert_eq!(
            (
                counts.members_read,
                counts.members_skipped,
                counts.failed_items
            ),
            (2, 1, 1)
        );
        let skipped = world.member(&world.campaign.campaign_id).await?;
        assert!(
            !matches!(skipped.policy_state, Ok(Some(_))),
            "{:?}",
            skipped.policy_state
        );
        assert!(world.kept_of(&other.campaign_id).await?.is_some());
        Ok(())
    }

    /// A pipette-duckdb read that fails ends the round's reads: the member
    /// after it reads unreadable without one, and each is a failed item. The
    /// member whose read failed goes behind the other, so the next round reads
    /// the other first and files into it, though the read fails again.
    #[tokio::test]
    async fn a_failed_duckdb_read_ends_the_rounds_reads_and_goes_behind() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let (other, other_jobs) = world
            .join("S26 NPU", &DOCUMENT.replace("4bit", "8bit"))
            .await?;
        assert!(world.campaign.campaign_id < other.campaign_id);
        world.turn_on(&[world.on(PolicyKind::Swap, 3)]).await?;
        let mine = world.jobs.first().context("a job")?;
        let theirs = other_jobs.first().context("a job")?;
        world.finish(&[mine, theirs]).await?;
        world.maintain().await?;
        let failing = world
            .duckdb
            .mock_async(|when, then| {
                when.method(POST).body_includes(format!("'{mine}'"));
                then.status(500);
            })
            .await;
        let theirs_read = world
            .answer(&[(theirs.as_str(), run(theirs.as_str(), &[PolicyKind::Swap]))])
            .await;
        let (config, groups) = (world.config(), world.groups()?);
        let first = round(&config, &world.stores, groups, Instant::now(), &probed).await?;
        failing.assert_calls_async(1).await;
        theirs_read.assert_calls_async(0).await;
        let counted = |counts: &PassCounts| {
            (
                counts.members_read,
                counts.members_unreadable,
                counts.revisions,
                counts.failed_items,
            )
        };
        assert_eq!(counted(&first), (2, 2, 0, 2));
        let behind = world.kept().await?.context("the failed read's record")?;
        assert!(behind.unreadable_at.is_some());
        assert_eq!(world.kept_of(&other.campaign_id).await?, None);

        let second = round(&config, &world.stores, groups, Instant::now(), &probed).await?;
        failing.assert_calls_async(2).await;
        theirs_read.assert_calls_async(1).await;
        assert_eq!(counted(&second), (2, 1, 1, 1));
        let filed = world.kept_of(&other.campaign_id).await?;
        assert!(filed.and_then(|record| record.filed).is_some());
        Ok(())
    }

    /// A group with a policy or a record that does not decode is skipped, a
    /// failed item that fails the run.
    #[rstest::rstest]
    #[case::policy("UPDATE campaign_group_policies SET body = 'not a policy'")]
    #[case::group("UPDATE campaign_groups SET body = 'not a group'")]
    #[tokio::test]
    async fn a_group_that_does_not_read_is_skipped(
        #[case] unreadable: &'static str,
    ) -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        world.turn_on(&[world.on(PolicyKind::Swap, 3)]).await?;
        world.db.execute(unreadable).await?;
        let counts = round(
            &world.config(),
            &world.stores,
            world.groups()?,
            Instant::now(),
            &probed,
        )
        .await?;
        assert_eq!(
            (counts.groups_skipped, counts.members, counts.failed_items),
            (1, 0, 1)
        );
        let ran = apply(&world.config(), &world.stores, Instant::now(), probed).await;
        let error = format!("{:#}", ran.err().context("the run passed")?);
        assert!(error.contains("1 policy pass item(s) failed"), "{error}");
        Ok(())
    }

    /// A row lacks model params when the pages would show it without a total
    /// or active count: a recorded model the table has no entry for, or a
    /// stored total with no active count. A model the table fills and stored
    /// counts do not. Each lacking row counts under the key of its model
    /// descriptor, else of its vault descriptor, so two quant files of one
    /// model share a key, and a row that records no model is left out. The
    /// check flags and keeps each job with such a row.
    #[tokio::test]
    async fn a_row_lacks_model_params_where_the_pages_show_no_count() -> anyhow::Result<()> {
        let Some(world) = World::new().await? else {
            return Ok(());
        };
        let jobs = [
            "job-1", "job-2", "job-3", "job-4", "job-5", "job-6", "job-7",
        ];
        let head: Vec<(&str, Option<&str>)> = jobs.iter().map(|job_id| (*job_id, None)).collect();
        world.put(&[world.plan(HEAD, &head)?]).await?;
        let granite = r#"{"type":"gguf_text","source":"huggingface","org":"ibm-granite","repo_name":"granite-4.2-8b-GGUF","path":"granite-4.2-8b-Q4_0.gguf"}"#;
        let other_quant = r#"{"type":"gguf_text","source":"relative_file","path":"models/granite-4.2-8b-Q5_K_M.gguf"}"#;
        let filled = r#"{"type":"mlx","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-MLX-4bit"}"#;
        let vault =
            r#"{"type":"mlx","source":"huggingface","org":"acme","repo_name":"acme-3b-MLX-4bit"}"#;
        let rows = [
            ("job-1", modeled("job-1", Some(granite), None, None, None)),
            ("job-1", modeled("job-1", Some(granite), None, None, None)),
            (
                "job-2",
                modeled("job-2", Some(other_quant), None, None, None),
            ),
            ("job-3", modeled("job-3", Some(filled), None, None, None)),
            (
                "job-4",
                modeled("job-4", Some(granite), None, Some(8000), Some(8000)),
            ),
            (
                "job-5",
                modeled("job-5", Some(granite), None, Some(8000), None),
            ),
            (
                "job-6",
                modeled(
                    "job-6",
                    Some(r#"{"type":"partner"}"#),
                    Some(vault),
                    None,
                    None,
                ),
            ),
            ("job-7", modeled("job-7", None, None, None, None)),
        ]
        .map(|(job_id, row)| (job_id, vec![row]));
        world.answer_in(&MODEL_COLUMNS, &rows).await;
        let table = ModelParamTable::from_toml("\"LFM2-700M\" = 742")?;
        let stored = [world.on(PolicyKind::ModelParams, 0)];
        let evaluation = world
            .round_with(&world.campaign, &stored, None, Some(&table))
            .await?;
        assert_eq!(evaluation.state, PolicyMemberState::Evaluated);
        let missing = |key: &str, row_count, job_count| ModelParamsGap {
            key: key.to_owned(),
            row_count,
            job_count,
        };
        assert_eq!(
            evaluation.missing_model_params,
            [missing("acme-3b", 1, 1), missing("granite-4.2-8b", 4, 3)]
        );
        assert_eq!(
            counts(&evaluation, PolicyKind::ModelParams),
            Some((4, 0, 0, 4))
        );
        assert_eq!(world.outcomes(&evaluation), Vec::<String>::new());
        Ok(())
    }

    /// With model_params alone on, a round reads the rows and counts the key
    /// its rows lack, files nothing, marks nothing, and keeps the job decided,
    /// with the policy triggered. Once an entry for the model is added, the
    /// next round clears it without reading the rows again.
    #[tokio::test]
    async fn model_params_alone_files_and_marks_nothing() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let stored = [world.on(PolicyKind::ModelParams, 0)];
        world.turn_on(&stored).await?;
        let answered = world.jobs.first().context("a job")?;
        world.finish(&[answered]).await?;
        world.maintain().await?;
        // The table the migrations seed has no entry for it.
        let model = r#"{"type":"gguf_text","source":"relative_file","path":"models/granite-4.2-8b-Q4_0.gguf"}"#;
        let row = modeled(answered.as_str(), Some(model), None, None, None);
        world
            .answer_in(&MODEL_COLUMNS, &[(answered.as_str(), vec![row])])
            .await;
        let (evaluation, filing, pass) = world.pass(&stored).await?;
        assert_eq!(
            (
                evaluation.state,
                evaluation.waiting_job_count,
                pass.failed_items
            ),
            (PolicyMemberState::Evaluated, 0, 0)
        );
        assert!(filing.is_none(), "{filing:?}");
        assert_eq!(world.outcomes(&evaluation), Vec::<String>::new());
        assert_eq!(evaluation.kinds, BTreeSet::new());
        assert_eq!(
            counts(&evaluation, PolicyKind::ModelParams),
            Some((1, 0, 0, 1))
        );
        assert_eq!(
            pass.models_missing_params,
            BTreeSet::from(["granite-4.2-8b".to_owned()])
        );
        let kept = world.kept().await?.context("the round's record")?;
        assert_eq!(
            (kept.policies, world.decided().await?, kept.triggered),
            (
                stored.to_vec(),
                BTreeSet::from([answered.clone()]),
                BTreeSet::from([PolicyKind::ModelParams])
            )
        );
        assert_eq!(world.plan_count().await?, 1);

        world
            .stores
            .model_params
            .put_model_params(&pipette_store_testkit::a_model_params(
                "granite-4.2-8b",
                8000,
            )?)
            .await?;
        // No query is answered now, so a round that read rows would read
        // unreadable.
        world.duckdb.reset_async().await;
        let (cleared, _, _) = world.pass(&stored).await?;
        assert_eq!(cleared.state, PolicyMemberState::Evaluated);
        let kept = world.kept().await?.context("the round's record")?;
        let flagged: Vec<BTreeSet<PolicyKind>> = world
            .job_states()
            .await?
            .into_iter()
            .map(|v| v.flagged)
            .collect();
        assert_eq!(
            (kept.triggered, flagged),
            (BTreeSet::new(), vec![BTreeSet::new()])
        );
        Ok(())
    }

    /// The preview still counts the rows that lack model params of a member
    /// that waits for devices, and shows it waiting.
    #[tokio::test]
    async fn the_preview_counts_a_waiting_members_rows_that_lack_model_params() -> anyhow::Result<()>
    {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        let [swapped, answered, ..] = world.jobs.as_slice() else {
            anyhow::bail!("five jobs, not {:?}", world.jobs);
        };
        let stored = [world.on(PolicyKind::Swap, 3)];
        world.turn_on(&stored).await?;
        world.finish(&[swapped, answered]).await?;
        world.maintain().await?;
        world
            .answer(&[
                (swapped.as_str(), run(swapped.as_str(), &[PolicyKind::Swap])),
                (answered.as_str(), run(answered.as_str(), &[])),
            ])
            .await;
        let (_, filing, _) = world.pass(&stored).await?;
        anyhow::ensure!(filing.is_some(), "the swapped job's filing");
        world.maintain().await?;
        world.duckdb.reset_async().await;
        let model = r#"{"type":"gguf_text","source":"relative_file","path":"models/granite-4.2-8b-Q4_0.gguf"}"#;
        let row = modeled(answered.as_str(), Some(model), None, None, None);
        world
            .answer_in(&MODEL_COLUMNS, &[(answered.as_str(), vec![row])])
            .await;
        let table = ModelParamTable::from_toml("\"LFM2-700M\" = 742")?;
        let preview = world
            .round_with(&world.current().await?, &stored, None, Some(&table))
            .await?;
        let params = counts(&preview, PolicyKind::ModelParams);
        assert_eq!(
            (preview.state, preview.missing_model_params, params),
            (
                PolicyMemberState::WaitingForDevices,
                vec![ModelParamsGap {
                    key: "granite-4.2-8b".to_owned(),
                    row_count: 1,
                    job_count: 1,
                }],
                Some((1, 0, 0, 1))
            )
        );
        Ok(())
    }

    /// A model params table that does not read skips the member this round, a
    /// failed item, and keeps nothing, so the next round reads its jobs again.
    #[tokio::test]
    async fn a_model_params_table_that_does_not_read_skips_the_member() -> anyhow::Result<()> {
        let Some(world) = World::filed().await? else {
            return Ok(());
        };
        world
            .turn_on(&[world.on(PolicyKind::ModelParams, 0)])
            .await?;
        world.db.execute("DROP TABLE model_params").await?;
        let counts = round(
            &world.config(),
            &world.stores,
            world.groups()?,
            Instant::now(),
            &probed,
        )
        .await?;
        assert_eq!(
            (
                counts.members_read,
                counts.members_skipped,
                counts.failed_items
            ),
            (1, 1, 1)
        );
        assert_eq!(world.kept().await?, None);
        Ok(())
    }
}
