//! The `POST /plans/claim` lease and the `/api/v1/plans` bodies. A malformed
//! stored body is leased as an [`UndecodableClaim`], which no device decodes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jobs::{PlanJob, ResultsRead};
use crate::{
    CampaignId, DescriptorDisplay, FleetDetail, JobId, ListOrder, NonEmptyTrimmedString, PlanId,
    UserId, UserPermission, VaultResourceId,
};

/// One leased job, as `POST /plans/claim` answers it. `spec` stays JSON as
/// stored; a client parses it as a [`ClientRunSpec`](crate::ClientRunSpec).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedJob {
    pub job_id: String,
    pub benchmark_id: String,
    /// The lease, as an ISO 8601 duration (`PT5M`).
    pub time_window: String,
    /// The job's deadline in ISO 8601 basic format; absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The client versions the job admits; empty when it names none.
    #[serde(default)]
    pub client_versions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
}

/// The job a claim leases out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClaimAnswer {
    Claimed(ClaimedJob),
    Undecodable(UndecodableClaim),
}

/// A lease on a body written straight into `avail/` without a string `job_id`
/// or `spec.benchmark`: [`ClaimedJob`]'s keys, with the ones the body cannot
/// fill left out rather than filled with something false. No device decodes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UndecodableClaim {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<Value>,
    pub time_window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub client_versions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
}

/// A plan's lifecycle state.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumIter,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PlanStatus {
    /// Jobs are being staged; nothing is claimable yet.
    Creating,
    /// At least one outstanding job is eligible to some working client.
    Active,
    /// Jobs are outstanding, but none matches any working client: a plan queued
    /// ahead of the fleet. Not an error.
    PendingClients,
    /// Teardown has started and live jobs remain. One-way: it ends `cancelled`.
    Cancelling,
    /// Every job reached a terminal state.
    Complete,
    /// Teardown is finished, and no live job remains.
    Cancelled,
}

impl PlanStatus {
    /// Whether this is a terminal latch maintenance never leaves.
    pub fn is_terminal(self) -> bool {
        matches!(self, PlanStatus::Complete | PlanStatus::Cancelled)
    }

    /// The serde spelling, for logs and display.
    pub fn label(self) -> &'static str {
        self.into()
    }
}

/// The `scope` a caller asks the plan listing for. Absent means the widest
/// the account may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RequestedScope {
    /// The plans of this account's campaigns and its organization's, and the
    /// ones it filed outside any campaign.
    Mine,
    /// Every plan. Refused with `403` without `read_any_plans`.
    All,
}

/// Query parameters for `GET /api/v1/plans`. An unknown key is a `400`, not an
/// unfiltered listing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PlanListQuery {
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub status: Option<PlanStatus>,
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub scope: Option<RequestedScope>,
    /// Narrow to the plans this account filed.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub submitted_by: Option<UserId>,
    /// Narrow to the plans filed into one campaign, within the scope: a
    /// campaign out of reach gives an empty listing rather than a `403`.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub campaign_id: Option<CampaignId>,
    /// Rows a page holds: 50 by default, at most 200 (the server's
    /// `list_default_limit` and `list_max_limit`). `0` or more than that is a
    /// `400`, not clamped.
    #[cfg_attr(feature = "schemars", schemars(range(min = 1)))]
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// The preceding page's `next_cursor`, unchanged; omit it for the first
    /// page.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub order: Option<ListOrder>,
    /// The submitters' addresses.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub names: Option<bool>,
}

/// What a read of one plan may add: its submitter's address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanReadQuery {
    /// The submitter's address.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub names: Option<bool>,
}

/// Query parameters for `GET /api/v1/plans/{plan_id}/jobs`. Spelled out rather
/// than flattened, which would lose the query string's string typing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanJobsQuery {
    /// Jobs a page holds: 100 by default, at most 500. `0` answers `total`
    /// alone.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// Jobs to skip, in the order the plan lists them.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub offset: Option<usize>,
    /// Each job's result, and what the plan's page offers beside them.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub results: Option<bool>,
    /// Every job in one page, whatever `limit` and `offset` say.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub all: Option<bool>,
}

/// The `GET /api/v1/jobs/{job_id}/plans` body: the plans in the caller's scope
/// that asked for one job, and the body of that job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JobPlans {
    /// Whether `body` carries the job's eligibility fields.
    pub fleet_detail: FleetDetail,
    /// Oldest first. A plan out of the scope is left out.
    pub plans: Vec<PlanSummary>,
    /// The job's shared fields, every `auth_token` dropped, and the eligibility
    /// fields dropped at the withheld tier. `null` when the job's source is
    /// gone.
    pub body: Option<Value>,
}

/// A plan's job counts. `finished` counts a job its client is done with,
/// scored or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Counts {
    pub total: u64,
    pub finished: u64,
    pub running: u64,
    pub available: u64,
    pub failed: u64,
    /// Jobs teardown retired, which hold no record. A job another plan shares
    /// counts here only once the last of its plans retires it.
    #[serde(default)]
    pub cancelled: u64,
}

/// Earliest and latest deadline across a plan's jobs, frozen at ingestion;
/// `null` for none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JobExpiry {
    pub earliest_expires_at: Option<DateTime<Utc>>,
    pub latest_expires_at: Option<DateTime<Utc>>,
}

/// Why ingestion flagged a [`Warning`]'s jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// The jobs match no working client; the message names the clauses.
    #[default]
    Fleet,
    /// The jobs name a model with no entry in the model parameter table.
    UncataloguedModel,
    /// A kind this build does not know, as written.
    #[serde(untagged)]
    Unknown(String),
}

/// A group of jobs ingestion flagged for one reason, frozen at ingestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Warning {
    #[serde(default)]
    pub kind: WarningKind,
    /// E.g. "2 jobs requiring os:macos match no working client".
    pub message: String,
    pub job_ids: Vec<JobId>,
}

impl Warning {
    /// The warning a reader at `detail` sees. A fleet message names clauses,
    /// and an unknown kind may, so the withheld tier reads a fixed sentence.
    pub fn at(mut self, detail: FleetDetail) -> Self {
        let may_describe_fleet = matches!(self.kind, WarningKind::Fleet | WarningKind::Unknown(_));
        if detail == FleetDetail::Withheld && may_describe_fleet {
            self.message = unmatched_warning(self.job_ids.len(), None);
        }
        self
    }
}

/// A starvation message, with the clauses when the reader may see them.
pub fn unmatched_warning(count: usize, clauses: Option<&str>) -> String {
    let (noun, verb) = if count == 1 {
        ("job", "matches")
    } else {
        ("jobs", "match")
    };
    let clauses = clauses.map(|text| format!(" {text}")).unwrap_or_default();
    format!("{count} {noun}{clauses} {verb} no working client")
}

/// The eligibility clauses a starved group shares. The three negatives are
/// omitted when empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StarvedClauses {
    /// Flags a client must all carry.
    pub requires: Vec<String>,
    /// Groups a client must carry at least one flag of each.
    pub any_of: Vec<Vec<String>>,
    /// Client ids listed outright.
    pub clients: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_clients: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_all_of: Vec<Vec<String>>,
}

/// A starved group at the caller's fleet tier: the clauses are withheld, the
/// job ids are not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StarvedGroup {
    #[serde(flatten)]
    pub clauses: Option<StarvedClauses>,
    pub job_ids: Vec<JobId>,
}

/// A plan's progress as maintenance last computed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ProgressSnapshot {
    pub computed_at: DateTime<Utc>,
    pub counts: Counts,
    pub starved_groups: Vec<StarvedGroup>,
}

impl ProgressSnapshot {
    /// The snapshot a reader at `detail` sees: the clauses describe the fleet,
    /// the job ids are the reader's own.
    pub fn at(mut self, detail: FleetDetail) -> Self {
        if detail == FleetDetail::Withheld {
            self.starved_groups
                .iter_mut()
                .for_each(|group| group.clauses = None);
        }
        self
    }
}

/// A plan, as the plan and campaign reads answer it and `plans status` prints
/// it. Its job ids are the jobs page's (`GET /api/v1/plans/{plan_id}/jobs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanView {
    pub fleet_detail: FleetDetail,
    pub plan_id: PlanId,
    /// Free text from the filer that tells why the plan was filed. A client
    /// changes the note with `PATCH /api/v1/plans/{plan_id}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
    /// What a campaign revision changed, as the server wrote it on filing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<RevisionChange>,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    /// The account the plan is attributed to; absent when no account filed it.
    /// Skipped rather than `null`: absence is a final answer here, where
    /// `null` below says "not computed yet".
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub submitted_by: Option<UserId>,
    /// The campaign the plan belongs to; absent for one filed outside any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign_id: Option<CampaignId>,
    /// The plan this one supersedes in its campaign.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_plan_id: Option<PlanId>,
    /// `null` for a plan filed before the field existed.
    pub job_expiry: Option<JobExpiry>,
    /// A cancel is requested but not yet latched into `status`, which takes up
    /// to one maintenance interval.
    pub cancel_requested: bool,
    /// The warnings frozen at ingestion. A fleet warning is the same fact as
    /// `progress_snapshot.starved_groups` at filing time; the two can disagree
    /// once clients register.
    pub warnings: Vec<Warning>,
    /// The planner's warnings on the plan document, which quote its clauses:
    /// empty at the withheld tier, and for a plan filed as job bodies.
    pub plan_warnings: Vec<String>,
    /// `null` until maintenance writes the first snapshot.
    pub progress_snapshot: Option<ProgressSnapshot>,
    /// When the plan latched `complete` or `cancelled`; `null` while it is live.
    pub terminal_at: Option<DateTime<Utc>>,
}

impl PlanView {
    /// The plan a reader at `detail` sees.
    pub fn at(mut self, detail: FleetDetail) -> Self {
        self.fleet_detail = detail;
        self.warnings = self.warnings.into_iter().map(|w| w.at(detail)).collect();
        // A planner warning quotes the document's clauses, so the tier that
        // withholds the document withholds them.
        if detail == FleetDetail::Withheld {
            self.plan_warnings.clear();
        }
        self.progress_snapshot = self.progress_snapshot.map(|s| s.at(detail));
        self
    }
}

/// A plan as a campaign row, a campaign's history and a job's plans draw it:
/// some of [`PlanView`]'s keys, valued alike, and its snapshot's `counts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanSummary {
    pub plan_id: PlanId,
    /// Free text from the filer that tells why the plan was filed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
    /// What a campaign revision changed, as the server wrote it on filing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<RevisionChange>,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    /// The account the plan is attributed to; absent when no account filed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub submitted_by: Option<UserId>,
    /// The campaign the plan belongs to; absent for one filed outside any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign_id: Option<CampaignId>,
    /// A cancel is requested but not yet latched into `status`.
    pub cancel_requested: bool,
    /// `null` until maintenance counts it.
    pub counts: Option<Counts>,
}

/// A filed plan's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanFiled {
    pub fleet_detail: FleetDetail,
    pub plan_id: PlanId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
    /// What a campaign revision changed, as the server wrote it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<RevisionChange>,
    /// Distinct jobs the plan holds: identical cells share one, so it can be
    /// fewer than `job_ids` has keys.
    pub job_count: usize,
    /// Cells that took an existing job instead of a new mint. The job is a
    /// queued job or an earlier finished job that answers the cell. A cell that
    /// takes the job of another plan counts in `job_count` too. A cell that
    /// takes the job of a sibling counts here only.
    pub merged_job_count: usize,
    /// An object from input label to job id, unlike the array `job_ids`
    /// elsewhere. In input order, which a schema cannot say.
    #[serde(with = "ordered_map")]
    #[cfg_attr(
        feature = "schemars",
        schemars(with = "std::collections::BTreeMap<String, JobId>")
    )]
    pub job_ids: Vec<(String, JobId)>,
    pub warnings: Vec<Warning>,
    /// The planner's warnings on the document, which every tier reads here.
    /// Omitted when empty, as it always is for a plan filed as job bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_warnings: Vec<String>,
}

impl PlanFiled {
    /// The report a reader at `detail` sees: its fleet warnings re-tiered.
    pub fn at(mut self, detail: FleetDetail) -> Self {
        self.fleet_detail = detail;
        self.warnings = self.warnings.into_iter().map(|w| w.at(detail)).collect();
        self
    }
}

/// One line of the progress stream on the routes that file a plan, for a caller
/// that sends `Accept: application/x-ndjson`. The stream ends with one `result`
/// or one `error` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(rename = "{B}IngestEvent"))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum IngestEvent<B> {
    /// The bytes of one file the import downloaded so far.
    Progress {
        /// The source of the file, as `org/repo_name@commit`. A model with
        /// sources in two repositories sends two values. The value names a
        /// source, not a model, so it differs from the
        /// [`PlannedImport::hf_ref`] of a dry run.
        hf_ref: String,
        /// The path in the repository.
        file: String,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// Sent when no other line went out for 15 seconds.
    Keepalive,
    /// A success: the status and body of the plain answer.
    Result { status: u16, body: B },
    /// A failure: the status and `error` message of the plain answer.
    Error { status: u16, error: String },
}

/// What a cancel request did; `already_complete` is a `409` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    Requested,
    AlreadyCancelling,
    AlreadyCancelled,
}

/// The `POST /api/v1/plans/{plan_id}/cancel` body: what the request did.
/// Maintenance latches the cancel, so the plan's `status` has not moved yet,
/// and the plan itself is read under `read_plans`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanCancelRequested {
    /// `requested` wrote the cancel marker. `already_cancelling` (maintenance
    /// is still retiring jobs) and `already_cancelled` wrote nothing.
    pub outcome: CancelOutcome,
    /// A sentence naming what happens next.
    pub message: String,
}

/// What the store holds of the document a plan was filed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DocumentState {
    Present,
    /// Filed as job bodies.
    #[serde(rename = "none")]
    NoDocument,
    Collected,
    /// The stored document does not match the plan's digest.
    Mismatch,
}

/// The `GET /api/v1/plans/{plan_id}/jobs` body: a page of the plan's jobs. Every
/// key after `jobs` is absent unless `results=true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanJobs {
    /// At `withheld`, the page names no device.
    pub fleet_detail: FleetDetail,
    /// The plan read.
    pub plan_id: PlanId,
    /// Jobs the manifest lists, before paging.
    pub total: usize,
    /// Jobs skipped before the page; `0` with `all=true`.
    pub offset: usize,
    /// The most jobs the page holds, once clamped; `total` with `all=true`.
    pub limit: usize,
    /// The page, in the order the plan was filed in, so paging is stable while
    /// the queue moves underneath.
    pub jobs: Vec<PlanJob>,
    /// The catalog names of each runtime digest and model digest that the
    /// jobs show, as this reader sees them. A digest without a readable name
    /// has no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor_names: Option<std::collections::BTreeMap<String, DescriptorDisplay>>,
    /// Whether the results store answered: when it did not, a job that may
    /// have a run reads `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results_read: Option<ResultsRead>,
    /// The campaign this plan heads, when the reader may re-run and
    /// recalculate its jobs there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerun_campaign_id: Option<CampaignId>,
    /// Whether the reader holds `cancel_permission`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub may_cancel: Option<bool>,
    /// The grant cancelling this plan takes from the reader.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_permission: Option<UserPermission>,
    /// Whether the reader may change the plan's note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub may_edit_note: Option<bool>,
    /// Below the full tier, `present` says only that the plan names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_state: Option<DocumentState>,
}

/// `PATCH /api/v1/plans/{plan_id}` request body. Unknown fields are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanRequest {
    /// The new note. The server trims it. A blank value clears the note. If
    /// the field is absent or `null`, the note does not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A dry run's report. It holds the job bodies that a document expands to, the
/// planner's warnings, and the import action for each gated model. No body
/// holds an `auth_token`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanDryRun {
    /// The job bodies, in the order a filing would stage them.
    pub bodies: Vec<Value>,
    /// The planner's warnings, as a filing reports them.
    pub plan_warnings: Vec<String>,
    /// One entry for each distinct model that carries a token. Empty when no
    /// model carries one.
    #[serde(default)]
    pub imports: Vec<PlannedImport>,
}

/// What the import of a submission does for one model, in a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlannedImport {
    /// The model's HuggingFace reference, without its token.
    pub hf_ref: String,
    #[serde(flatten)]
    pub action: ImportAction,
}

/// What the import does for one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ImportAction {
    /// The owner holds these records from an earlier import of the model at
    /// this commit.
    Reuse {
        #[cfg_attr(feature = "schemars", schemars(with = "Vec<String>"))]
        resource_ids: Vec<VaultResourceId>,
    },
    /// The import downloads and stores this many bytes.
    Fetch { bytes: u64 },
}

/// An ingest route's answer: a filed plan's report, or a dry run's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum PlanAnswer {
    Filed(PlanFiled),
    DryRun(PlanDryRun),
}

/// How serious a [`PlanIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
}

/// One fault or warning in a plan document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanIssue {
    pub severity: IssueSeverity,
    pub message: String,
    /// The 1-based line in the submitted text.
    pub line: Option<usize>,
    /// The 1-based column, in characters. Only a TOML fault has one.
    pub column: Option<usize>,
}

/// A plan document's check, without filing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanValidation {
    /// The jobs the document expands to, after equal bodies merge; `null` when
    /// it does not expand.
    pub job_count: Option<usize>,
    /// The distinct models that carry an `auth_token`; `null` when `job_count`
    /// is.
    pub gated_model_count: Option<usize>,
    /// Errors, then the planner's warnings in plan order.
    pub issues: Vec<PlanIssue>,
}

/// `(label, value)` pairs as a JSON object, in their order both ways.
mod ordered_map {
    use serde::de::{MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use serde::{Deserializer, Serializer};

    use crate::JobId;

    pub(super) fn serialize<S: Serializer>(
        pairs: &[(String, JobId)],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(pairs.len()))?;
        pairs
            .iter()
            .try_for_each(|(key, value)| map.serialize_entry(key, value))?;
        map.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<(String, JobId)>, D::Error> {
        struct Pairs;

        impl<'de> Visitor<'de> for Pairs {
            type Value = Vec<(String, JobId)>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object of job ids")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut pairs = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(pair) = map.next_entry()? {
                    pairs.push(pair);
                }
                Ok(pairs)
            }
        }

        d.deserialize_map(Pairs)
    }
}

/// The answered jobs of the head plan that a re-run takes. The record of each
/// job gives its result. `failed` takes each job with a failed result. `all`
/// takes every answered job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Answered {
    Failed,
    All,
}

/// One variant of a plan, which a variant edit's `restore` names as
/// `<plan_id>:<index>`. The index counts the variants as
/// `GET /api/v1/plans/{plan_id}/variants` lists them, from 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RestoredVariant {
    pub plan_id: PlanId,
    pub variant_index: usize,
}

/// Why a `<plan_id>:<index>` does not parse.
#[derive(Debug, thiserror::Error)]
pub enum RestoredVariantError {
    #[error("a restored variant is `<plan_id>:<index>`; {0:?} has no `:`")]
    NoIndex(String),
    #[error("the index of the restored variant {text:?} is not a number: {source}")]
    BadIndex {
        text: String,
        source: std::num::ParseIntError,
    },
    #[error(transparent)]
    PlanId(#[from] crate::IdError),
}

impl std::str::FromStr for RestoredVariant {
    type Err = RestoredVariantError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (plan_id, index) = s
            .rsplit_once(':')
            .ok_or_else(|| RestoredVariantError::NoIndex(s.to_string()))?;
        let variant_index = index
            .parse()
            .map_err(|source| RestoredVariantError::BadIndex {
                text: s.to_string(),
                source,
            })?;
        Ok(Self {
            plan_id: plan_id.parse()?,
            variant_index,
        })
    }
}

impl std::fmt::Display for RestoredVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.plan_id, self.variant_index)
    }
}

/// The JSON answer of `GET /api/v1/plans/{plan_id}/variants`: each variant of
/// the plan as its own standalone `[[variants]]` block, in the order that a
/// variant edit's `restore` numbers them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanVariants {
    pub variants: Vec<String>,
}

/// The change that a campaign revision made. The server records it at filing.
/// The value does not change after filing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RevisionChange {
    /// A variant is a `[[variants]]` block of the document. `restored` names
    /// the variants of other plans that the edit added, in the order added.
    Variants {
        removed_variant_count: usize,
        added_variant_count: usize,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        carry_earlier: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inherit_from: Vec<PlanId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restored: Vec<RestoredVariant>,
    },
    Document {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        carry_earlier: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inherit_from: Vec<PlanId>,
    },
    /// `answered` is absent when the filer named the jobs.
    Rerun {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answered: Option<Answered>,
        job_count: usize,
    },
    Recalculate {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all_evals: bool,
        job_count: usize,
    },
    /// A copy of the document of `to_plan_id`, an earlier plan of the
    /// campaign. It offers the finished runs of every earlier plan.
    Revert { to_plan_id: PlanId },
}

/// Return `n` and `noun`. Add an `s` to `noun` if `n` is not 1.
fn counted(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// Format the change as one line of text for a terminal. The web app uses its
/// own format.
impl std::fmt::Display for RevisionChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (carry_earlier, inherit_from) = match self {
            Self::Variants {
                removed_variant_count: removed,
                added_variant_count: added,
                carry_earlier,
                inherit_from,
                restored,
            } => {
                write!(f, "Variant edit: {removed} removed, {added} added.")?;
                for variant in restored {
                    write!(
                        f,
                        " Restored variant {} of {}.",
                        variant.variant_index, variant.plan_id
                    )?;
                }
                (*carry_earlier, inherit_from.as_slice())
            }
            Self::Document {
                carry_earlier,
                inherit_from,
            } => {
                f.write_str("Document revised.")?;
                (*carry_earlier, inherit_from.as_slice())
            }
            Self::Rerun {
                answered: None,
                job_count,
            } => return write!(f, "Re-run of {}.", counted(*job_count, "job")),
            Self::Rerun {
                answered: Some(Answered::Failed),
                job_count,
            } => return write!(f, "Re-run of all failed jobs ({job_count})."),
            Self::Rerun {
                answered: Some(Answered::All),
                job_count,
            } => return write!(f, "Re-run of all finished or failed jobs ({job_count})."),
            Self::Recalculate {
                all_evals: false,
                job_count,
            } => return write!(f, "Recalculation of {}.", counted(*job_count, "eval")),
            Self::Recalculate {
                all_evals: true,
                job_count,
            } => return write!(f, "Recalculation of all finished evals ({job_count})."),
            Self::Revert { to_plan_id } => return write!(f, "Reverted to {to_plan_id}."),
        };
        if carry_earlier {
            f.write_str(" Carries runs of earlier plans.")?;
        }
        if !inherit_from.is_empty() {
            let ids: Vec<&str> = inherit_from.iter().map(PlanId::as_str).collect();
            write!(f, " Inherits runs of {}.", ids.join(", "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod operator_tests {
    use super::*;

    /// The job id map keeps input order both ways, so `10` follows `9`.
    #[test]
    fn a_filed_plan_keeps_its_job_ids_in_input_order() -> anyhow::Result<()> {
        let jobs = (8..12)
            .map(|i| Ok((i.to_string(), JobId::try_new(format!("job-{i}"))?)))
            .collect::<Result<Vec<_>, crate::IdError>>()?;
        let filed = PlanFiled {
            fleet_detail: FleetDetail::Full,
            plan_id: PlanId::try_new("plan-1")?,
            note: None,
            change: None,
            job_count: 4,
            merged_job_count: 0,
            job_ids: jobs.clone(),
            warnings: Vec::new(),
            plan_warnings: Vec::new(),
        };
        let text = serde_json::to_string(&filed)?;
        assert!(
            text.contains(r#""job_ids":{"8":"job-8","9":"job-9","10":"job-10","11":"job-11"}"#)
        );
        let read: PlanFiled = serde_json::from_str(&text)?;
        assert_eq!(read.job_ids, jobs);
        Ok(())
    }

    /// A dry run reads back as one, though both arms are objects. Each import
    /// action reads back as its own.
    #[test]
    fn an_ingest_answer_reads_back_as_its_arm() -> anyhow::Result<()> {
        let dry = PlanAnswer::DryRun(PlanDryRun {
            bodies: Vec::new(),
            plan_warnings: vec!["w".into()],
            imports: vec![
                PlannedImport {
                    hf_ref: "LiquidAI/LFM2-700M-GGUF:m.gguf".into(),
                    action: ImportAction::Fetch { bytes: 12 },
                },
                PlannedImport {
                    hf_ref: "LiquidAI/LFM2-700M-MLX-4bit".into(),
                    action: ImportAction::Reuse {
                        resource_ids: vec![VaultResourceId::try_new(
                            "vault_0190a4b2-7c1e-7d3a-8f00-000000000001",
                        )?],
                    },
                },
            ],
        });
        let read: PlanAnswer = serde_json::from_str(&serde_json::to_string(&dry)?)?;
        assert_eq!(read, dry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::{Job, JobInPlan, JobQueue, JobResult, JobState, Lease, Named, ParentJob};

    fn job(job_id: &str, state: JobState, leased: bool) -> anyhow::Result<PlanJob> {
        let lease = Lease {
            expires_at: "2026-10-16T12:05:00Z".parse()?,
            client_id: None,
        };
        let job = Job {
            job_id: JobId::try_new(job_id)?,
            state,
            planned: None,
            queue: leased.then_some(JobQueue {
                expires_at: None,
                lease: Some(lease),
            }),
            plan: JobInPlan {
                lapsed: None,
                rerun_of: Some(ParentJob {
                    job_id: JobId::try_new("job-0")?,
                    plan_id: None,
                }),
                rerun_history: Vec::new(),
            },
        };
        Ok(PlanJob {
            job,
            result: None,
            planned_runtime_sha256: None,
            planned_model_sha256: None,
        })
    }

    fn unasked() -> anyhow::Result<Named<PlanJobs>> {
        let body = PlanJobs {
            fleet_detail: FleetDetail::Withheld,
            plan_id: PlanId::try_new("plan-1")?,
            total: 2,
            offset: 0,
            limit: 100,
            jobs: vec![
                job("job-1", JobState::Running, true)?,
                job("job-2", JobState::Finished, false)?,
            ],
            descriptor_names: None,
            results_read: None,
            rerun_campaign_id: None,
            may_cancel: None,
            cancel_permission: None,
            may_edit_note: None,
            document_state: None,
        };
        Ok(Named { body, names: None })
    }

    fn asked() -> anyhow::Result<Named<PlanJobs>> {
        let mut page = unasked()?.body;
        page.jobs
            .iter_mut()
            .zip([JobResult::NotRun, JobResult::Unknown])
            .for_each(|(row, result)| row.result = Some(result));
        Ok(Named {
            body: PlanJobs {
                results_read: Some(ResultsRead::TimedOut),
                rerun_campaign_id: Some(CampaignId::try_new(
                    "campaign_0195c3e2-0000-7000-8000-000000000001",
                )?),
                may_cancel: Some(true),
                cancel_permission: Some(UserPermission::CancelPlans),
                may_edit_note: Some(false),
                document_state: Some(DocumentState::Present),
                ..page
            },
            names: Some([("vault_1".to_string(), "weights".to_string())].into()),
        })
    }

    /// Unasked, the jobs body writes the page's keys alone; `results=true` adds
    /// its keys after them.
    #[rstest]
    #[case::unasked(unasked, r#"{"fleet_detail":"withheld","plan_id":"plan-1","total":2,"offset":0,"limit":100,"jobs":[{"job_id":"job-1","state":"running","planned":null,"queue":{"expires_at":null,"lease":{"expires_at":"2026-10-16T12:05:00Z","client_id":null}},"plan":{"lapsed":null,"rerun_of":{"job_id":"job-0","plan_id":null},"rerun_history":[]}},{"job_id":"job-2","state":"finished","planned":null,"queue":null,"plan":{"lapsed":null,"rerun_of":{"job_id":"job-0","plan_id":null},"rerun_history":[]}}]}"#)]
    #[case::asked(asked, r#"{"fleet_detail":"withheld","plan_id":"plan-1","total":2,"offset":0,"limit":100,"jobs":[{"job_id":"job-1","state":"running","planned":null,"queue":{"expires_at":null,"lease":{"expires_at":"2026-10-16T12:05:00Z","client_id":null}},"plan":{"lapsed":null,"rerun_of":{"job_id":"job-0","plan_id":null},"rerun_history":[]},"result":{"status":"not_run"}},{"job_id":"job-2","state":"finished","planned":null,"queue":null,"plan":{"lapsed":null,"rerun_of":{"job_id":"job-0","plan_id":null},"rerun_history":[]},"result":{"status":"unknown"}}],"results_read":"timed_out","rerun_campaign_id":"campaign_0195c3e2-0000-7000-8000-000000000001","may_cancel":true,"cancel_permission":"cancel_plans","may_edit_note":false,"document_state":"present","names":{"vault_1":"weights"}}"#)]
    fn each_form_of_the_jobs_body_keeps_its_bytes(
        #[case] form: fn() -> anyhow::Result<Named<PlanJobs>>,
        #[case] bytes: &str,
    ) -> anyhow::Result<()> {
        let body = form()?;
        let written = serde_json::to_string(&body)?;
        assert_eq!(written, bytes);
        assert_eq!(serde_json::from_str::<Named<PlanJobs>>(&written)?, body);
        Ok(())
    }

    /// A warning stored before `kind` existed reads as a fleet warning, so its
    /// message stays withheld from a caller that may not read the fleet.
    #[test]
    fn warning_without_a_kind_reads_as_fleet() -> anyhow::Result<()> {
        let warning: Warning = serde_json::from_value(json!({
            "message": "2 jobs requiring os:macos match no working client",
            "job_ids": ["job-1", "job-2"],
        }))?;
        assert_eq!(warning.kind, WarningKind::Fleet);
        assert_eq!(
            warning.at(FleetDetail::Withheld).message,
            "2 jobs match no working client"
        );
        Ok(())
    }

    /// A later build's warning kind reads as `Unknown`, writes back unchanged,
    /// and keeps its message from a caller that may not read the fleet.
    #[test]
    fn warning_of_an_unknown_kind_reads_as_unknown() -> anyhow::Result<()> {
        let stored = json!({
            "kind": "a_kind_from_a_later_build",
            "message": "2 jobs requiring os:macos match no working client",
            "job_ids": ["job-1", "job-2"],
        });
        let warning: Warning = serde_json::from_value(stored.clone())?;
        assert_eq!(
            warning.kind,
            WarningKind::Unknown("a_kind_from_a_later_build".into())
        );
        assert_eq!(serde_json::to_value(&warning)?, stored);
        // A known kind still reads as itself, not as `Unknown`.
        let known: WarningKind = serde_json::from_value(json!("uncatalogued_model"))?;
        assert_eq!(known, WarningKind::UncataloguedModel);
        assert_eq!(
            warning.at(FleetDetail::Withheld).message,
            "2 jobs match no working client"
        );
        Ok(())
    }
}

#[cfg(test)]
mod revision_change_tests {
    use super::*;

    enum SentenceCase {
        RerunOne,
        RerunFailed,
        Recalculate,
        VariantsCarry,
        VariantsRestored,
        Revert,
    }

    /// The app's `changeText` (`ui/src/lib/plans.ts`) prints the same sentences.
    #[rstest::rstest]
    #[case::rerun_one(SentenceCase::RerunOne, "Re-run of 1 job.")]
    #[case::rerun_failed(SentenceCase::RerunFailed, "Re-run of all failed jobs (12).")]
    #[case::recalculate(SentenceCase::Recalculate, "Recalculation of 2 evals.")]
    #[case::variants_carry(
        SentenceCase::VariantsCarry,
        "Variant edit: 1 removed, 0 added. Carries runs of earlier plans. \
         Inherits runs of plan-a, plan-b."
    )]
    #[case::variants_restored(
        SentenceCase::VariantsRestored,
        "Variant edit: 1 removed, 2 added. Restored variant 2 of plan-a. \
         Restored variant 0 of plan-a. Inherits runs of plan-a."
    )]
    #[case::revert(SentenceCase::Revert, "Reverted to plan-a.")]
    fn a_revision_change_reads_as_a_sentence(
        #[case] case: SentenceCase,
        #[case] text: &str,
    ) -> anyhow::Result<()> {
        let change = match case {
            SentenceCase::RerunOne => RevisionChange::Rerun {
                answered: None,
                job_count: 1,
            },
            SentenceCase::RerunFailed => RevisionChange::Rerun {
                answered: Some(Answered::Failed),
                job_count: 12,
            },
            SentenceCase::Recalculate => RevisionChange::Recalculate {
                all_evals: false,
                job_count: 2,
            },
            SentenceCase::VariantsCarry => RevisionChange::Variants {
                removed_variant_count: 1,
                added_variant_count: 0,
                carry_earlier: true,
                inherit_from: vec![PlanId::try_new("plan-a")?, PlanId::try_new("plan-b")?],
                restored: Vec::new(),
            },
            SentenceCase::VariantsRestored => RevisionChange::Variants {
                removed_variant_count: 1,
                added_variant_count: 2,
                carry_earlier: false,
                inherit_from: vec![PlanId::try_new("plan-a")?],
                restored: vec![
                    RestoredVariant {
                        plan_id: PlanId::try_new("plan-a")?,
                        variant_index: 2,
                    },
                    RestoredVariant {
                        plan_id: PlanId::try_new("plan-a")?,
                        variant_index: 0,
                    },
                ],
            },
            SentenceCase::Revert => RevisionChange::Revert {
                to_plan_id: PlanId::try_new("plan-a")?,
            },
        };
        assert_eq!(change.to_string(), text);
        Ok(())
    }

    /// Tagged by `type`, counts named `_count`, with the defaults left out. The
    /// SQL store keeps its own stored form.
    #[rstest::rstest]
    #[case::document(
        RevisionChange::Document { carry_earlier: false, inherit_from: Vec::new() },
        serde_json::json!({"type": "document"})
    )]
    #[case::variants(
        RevisionChange::Variants {
            removed_variant_count: 1,
            added_variant_count: 2,
            carry_earlier: false,
            inherit_from: Vec::new(),
            restored: Vec::new()
        },
        serde_json::json!({
            "type": "variants", "removed_variant_count": 1, "added_variant_count": 2
        })
    )]
    #[case::restored_variants(
        RevisionChange::Variants {
            removed_variant_count: 0,
            added_variant_count: 1,
            carry_earlier: false,
            inherit_from: Vec::new(),
            restored: vec![RestoredVariant {
                plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(1)),
                variant_index: 3
            }]
        },
        serde_json::json!({
            "type": "variants", "removed_variant_count": 0, "added_variant_count": 1,
            "restored": [{
                "plan_id": "plan-00000000-0000-0000-0000-000000000001", "variant_index": 3
            }]
        })
    )]
    #[case::rerun(
        RevisionChange::Rerun { answered: None, job_count: 3 },
        serde_json::json!({"type": "rerun", "job_count": 3})
    )]
    #[case::recalculate(
        RevisionChange::Recalculate { all_evals: false, job_count: 4 },
        serde_json::json!({"type": "recalculate", "job_count": 4})
    )]
    #[case::revert(
        RevisionChange::Revert { to_plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(1)) },
        serde_json::json!({"type": "revert", "to_plan_id": "plan-00000000-0000-0000-0000-000000000001"})
    )]
    fn a_revision_change_is_tagged_by_type(
        #[case] change: RevisionChange,
        #[case] wire: serde_json::Value,
    ) -> anyhow::Result<()> {
        let value = serde_json::to_value(&change)?;
        assert_eq!(value, wire);
        assert_eq!(serde_json::from_value::<RevisionChange>(value)?, change);
        Ok(())
    }

    /// `<plan_id>:<index>` parses to its parts and prints back the same.
    #[rstest::rstest]
    #[case::good("plan-a:2", Some(("plan-a", 2)))]
    #[case::no_colon("plan-a", None)]
    #[case::bad_index("plan-a:two", None)]
    #[case::negative_index("plan-a:-1", None)]
    #[case::empty_index("plan-a:", None)]
    #[case::bad_plan_id("plan_a:2", None)]
    #[case::empty_plan_id(":2", None)]
    fn a_restored_variant_parses_from_plan_and_index(
        #[case] text: &str,
        #[case] parts: Option<(&str, usize)>,
    ) -> anyhow::Result<()> {
        let parsed = text.parse::<RestoredVariant>();
        match parts {
            Some((plan_id, variant_index)) => {
                let parsed = parsed?;
                assert_eq!(
                    parsed,
                    RestoredVariant {
                        plan_id: PlanId::try_new(plan_id)?,
                        variant_index,
                    }
                );
                assert_eq!(parsed.to_string(), text);
            }
            None => assert!(parsed.is_err(), "{text} is refused"),
        }
        Ok(())
    }
}
