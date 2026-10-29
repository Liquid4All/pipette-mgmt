//! `/api/v1/campaigns`: a campaign, the plans it filed, what its writes did,
//! and its revisions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::owners::PrincipalOwner;
use crate::plans::{Answered, PlanDryRun, PlanFiled, PlanSummary, RestoredVariant, RevisionChange};
use crate::{
    CampaignId, Changed, ElapsedMillis, JobId, Labels, NonEmptyTrimmedString, OrgId, PlanId,
    VaultPrincipalId,
};

/// A campaign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Campaign {
    /// `campaign_` and a UUIDv7; it never changes.
    pub campaign_id: CampaignId,
    /// What to print for a human; nothing keys on it, and two campaigns may
    /// share one.
    pub name: NonEmptyTrimmedString,
    /// Whose campaign it is.
    pub owner: PrincipalOwner,
    /// Absent only on a record written before a campaign was created with its
    /// first plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_plan_id: Option<PlanId>,
    /// What its readers group and filter it by. Absent on a record nobody
    /// labelled.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    /// Absent while `active`. Beside the plan's `status`, which is how far
    /// the work has run.
    #[serde(default, skip_serializing_if = "CampaignState::is_active")]
    pub state: CampaignState,
    pub created_at: DateTime<Utc>,
    /// Moves with each write that changes the record: a filing that moves
    /// `current_plan_id`, a rename, a relabel, a state change and a hand-off.
    pub updated_at: DateTime<Utc>,
}

/// Whether a campaign is in use. An archived one is hidden from its readers'
/// listings by default and takes no new plan until it is unarchived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CampaignState {
    #[default]
    Active,
    Archived,
}

impl CampaignState {
    pub fn is_active(&self) -> bool {
        *self == Self::Active
    }
}

impl std::str::FromStr for CampaignState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            other => Err(format!(
                "unknown state `{other}`, expected `active` or `archived`"
            )),
        }
    }
}

/// The plans a campaign has run, newest first. Ids rather than rows: a caller
/// wanting a plan's state reads the plan, under its own scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignPlans {
    /// What the campaign runs, or `null` for a record written before a
    /// campaign needed a first plan.
    pub current_plan_id: Option<PlanId>,
    /// What that supersedes, newest first.
    pub superseded_plan_ids: Vec<PlanId>,
    /// The walk stopped at its ceiling with revisions still behind it.
    pub history_truncated: bool,
}

/// A created campaign, and the report on its first plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignCreated {
    #[serde(flatten)]
    pub campaign: Campaign,
    pub plan: PlanFiled,
}

/// What a revision did to the plan the campaign ran before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Revised {
    /// The plan the campaign ran before the revision.
    pub previous_plan_id: PlanId,
    /// Its jobs the new plan holds.
    pub carried_job_count: usize,
    /// Its jobs the new plan runs again as new jobs.
    pub rerun_job_count: usize,
    /// Its jobs whose completions the new plan scores again as new jobs.
    pub recalculated_job_count: usize,
    /// Its jobs the new plan neither holds nor runs again.
    pub dropped_job_count: usize,
    /// `false` when it had already ended, or was already being cancelled.
    pub cancel_requested: bool,
}

/// A campaign revised by a variant edit, a document revision, a re-run or a
/// recalculation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignRevised {
    #[serde(flatten)]
    pub revised: Revised,
    pub plan: PlanFiled,
}

/// What a revision would do, without filing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RevisionPreview {
    /// The document is the head's own, so filing it files nothing. Only a
    /// document revision or a revert can be. Every count is then 0, and no
    /// check or import runs.
    pub unchanged: bool,
    /// The head's jobs the revision would hold, finished ones included.
    pub carried_job_count: usize,
    /// Cells the head never ran, which the revision would queue.
    pub new_job_count: usize,
    /// Cells the head ran under other requirements, which the revision would
    /// run again: the run failed, or its device or build is not known to meet them.
    pub requirements_rerun_job_count: usize,
    /// Cells that an earlier finished run answers, which the revision would
    /// hold that run for and not queue.
    #[serde(default)]
    pub satisfied_job_count: usize,
    /// Finished jobs of the campaign's earlier plans the revision would hold
    /// (`carry_earlier`).
    pub restored_job_count: usize,
    /// Finished jobs of the `inherit_from` plans the revision would hold.
    #[serde(default)]
    pub inherited_job_count: usize,
    pub rerun_job_count: usize,
    pub recalculated_job_count: usize,
    /// The head's jobs the revision would stop.
    pub dropped_job_ids: Vec<JobId>,
    /// Each body that would hold an existing job, and the plan the job comes
    /// from. A body not listed queues a new job or takes an earlier finished
    /// run (`satisfied_job_count`).
    #[serde(default)]
    pub held_jobs: Vec<HeldJob>,
    /// Indices of the revised document's variants that repeat an earlier one
    /// once each holds its benchmark list, so they add no job.
    #[serde(default)]
    pub repeated_variants: Vec<usize>,
}

/// A job a revision would hold for one of its bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct HeldJob {
    /// Its index in the dry run's `bodies`.
    pub body_index: usize,
    /// The head, a plan before it, or an `inherit_from` plan.
    pub plan_id: PlanId,
    pub job_id: JobId,
}

/// A revision's dry run: the document it would file, with every model token
/// dropped, its expansion, and what it would do to the head.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RevisionDryRun {
    pub document: String,
    #[serde(flatten)]
    pub report: PlanDryRun,
    #[serde(flatten)]
    pub preview: RevisionPreview,
}

/// The answer of `POST /api/v1/campaigns`: the created campaign, or a dry
/// run's report. The progress stream carries it as the `body` of its `result`
/// line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum CampaignAnswer {
    Created(Box<CampaignCreated>),
    DryRun(PlanDryRun),
}

/// The answer of a revision by a variant edit or by a whole document. It is
/// the filed revision, a dry run, or the campaign unchanged. The campaign is
/// unchanged when the document is the head's own. The progress stream carries
/// it as the `body` of its `result` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum RevisionAnswer {
    Revised(CampaignRevised),
    DryRun(RevisionDryRun),
    Unchanged(Changed<CampaignView>),
}

/// `PATCH /api/v1/campaigns/{campaign_id}` request body: each field present is
/// set, `labels` as the whole set, `state` to archive or unarchive it. The
/// owner has its own route, and no route changes the plan. Unknown fields are
/// refused, so an `owner` sent here is a `400` rather than ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateCampaignRequest {
    /// At most 4096 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<CampaignState>,
}

/// `PUT /api/v1/campaigns/{campaign_id}/owner` request body. Its own route,
/// because a hand-off checks both ends. Not nullable: a campaign always has an
/// owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetCampaignOwnerRequest {
    pub owner: PrincipalOwner,
}

/// `POST /api/v1/campaigns/{campaign_id}/rerun` request body: `job_ids` or
/// `answered`. Unknown keys are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RerunJobsRequest {
    /// Jobs of the plan the campaign runs, each finished or failed. A job
    /// named twice is re-run once.
    #[serde(
        default,
        deserialize_with = "crate::parsed_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub job_ids: Option<Vec<JobId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered: Option<Answered>,
    /// The revision's note, as on `POST /api/v1/campaigns/{campaign_id}/plans`.
    /// The server also records the change that the revision makes.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
}

/// `POST /api/v1/campaigns/{campaign_id}/recalculate` request body: `job_ids`
/// or `all_evals`. Unknown keys are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RecalculateJobsRequest {
    /// Jobs of the plan the campaign runs, each an eval that finished.
    #[serde(
        default,
        deserialize_with = "crate::parsed_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub job_ids: Option<Vec<JobId>>,
    /// Every job of the head whose record holds completions to score again.
    #[serde(default)]
    pub all_evals: bool,
    /// The revision's note, as on `POST /api/v1/campaigns/{campaign_id}/plans`.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
}

/// `POST /api/v1/campaigns/{campaign_id}/revert` request body. Unknown keys
/// are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RevertCampaignRequest {
    /// An earlier plan of the campaign, whose document the revision copies.
    /// `404` if the campaign's history does not hold it.
    #[serde(deserialize_with = "crate::parsed")]
    pub plan_id: PlanId,
    /// The revision's note, as on `POST /api/v1/campaigns/{campaign_id}/plans`.
    /// The server also records the change that the revision makes.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
}

/// The plan a revision is read from, which it names as `base_plan_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RevisionBase {
    pub plan_id: PlanId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<RevisionChange>,
}

/// The `…/variants` body: the plan a variant edit revises, and its
/// `[[variants]]` blocks in the order `remove` numbers them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignVariants {
    pub base: RevisionBase,
    pub variants: Vec<String>,
}

/// The `…/document` body: the plan a document revision replaces, and the
/// document it was filed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignDocument {
    pub base: RevisionBase,
    pub document: String,
}

/// A campaign as its listing, its GET and the writes that answer the record alone
/// answer it; with `plan=true`, a read adds what the plan it runs is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignView {
    #[serde(flatten)]
    pub campaign: Campaign,
    /// Absent unasked, for no plan, and for one outside the reader's plan scope
    /// or whose manifest does not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSummary>,
}

/// The device time a set of plans spent, each job at its latest scoring run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DeviceTimeTotal {
    /// The sum over the scored jobs that reported a value; `null` when none
    /// did.
    pub device_time_ms: Option<ElapsedMillis>,
    /// The scored jobs whose client reported no value, which the sum leaves
    /// out. A failure the server declared is in neither.
    pub unreported_job_count: usize,
}

/// One plan of a campaign's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignPlanRow {
    pub plan_id: PlanId,
    /// `null` when this reader may not read the plan, or its manifest is gone;
    /// its cancel marker is not read.
    pub plan: Option<PlanSummary>,
}

/// The `GET /api/v1/campaigns/{campaign_id}/overview` body: the campaign, what
/// the reader may do to it, and the plans it has run. Its [`Named`](crate::Named)
/// always holds the owner's name and the address of each submitter `history`
/// shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignOverview {
    #[serde(flatten)]
    pub campaign: Campaign,
    /// Whether the reader may rename it and hand it over.
    pub may_administer: bool,
    /// Whether a revision would admit this reader; the variant edit takes the
    /// same gate.
    pub may_edit_variants: bool,
    /// The same for a re-run, which also needs the plan's content digests.
    pub may_rerun: bool,
    /// The organization where this reader may create a descriptor catalog
    /// entry for the campaign's descriptors. The organization is the owner, or
    /// the current organization of an owning user. The reader must belong to
    /// it and hold `edit_catalog`, or belong to no organization and hold
    /// `edit_any_catalog`. The value is `null` for every other reader. It is
    /// also `null` when the server has no `[database]` catalog.
    pub catalog_org_id: Option<OrgId>,
    /// The plans this campaign has run, newest first.
    pub history: Vec<CampaignPlanRow>,
    /// The walk stopped at its ceiling of 50 plans, so the history is partial.
    pub history_truncated: bool,
    /// The device time of the current plan's jobs, re-scored copies included.
    /// `null` unless `device_time=true` asks, without `[duckdb]`, for a plan
    /// out of the reader's scope or gone, and when pipette-duckdb fails or
    /// takes over 3 s (or `[duckdb] timeout_secs`, if shorter).
    pub device_time_current_plan: Option<DeviceTimeTotal>,
    /// The device time of every job of `history`, each distinct job once and
    /// re-scored copies left out. `null` too when the history is truncated.
    pub device_time_all_plans: Option<DeviceTimeTotal>,
}

/// The query string of `POST /api/v1/campaigns`, whose body is the first
/// plan's document. Unknown keys are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateCampaignQuery {
    /// At most 4096 characters.
    pub name: NonEmptyTrimmedString,
    /// Who owns the campaign, a `user_…` or `org_…` id; absent, whoever signs
    /// the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<VaultPrincipalId>,
    /// The first plan's note, as on `POST /api/v1/campaigns/{campaign_id}/plans`.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
    /// Expand and validate the document, and create nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// The query string of `POST /api/v1/campaigns/{campaign_id}/plans`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CampaignPlanDocumentQuery {
    /// The note of the plan. The server trims it. The note has at most 4096
    /// characters. A blank note counts as no note.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
    /// Expand and validate, stage nothing, and leave the campaign as it is.
    #[serde(default)]
    pub dry_run: bool,
}

/// The query string of `…/variants` and `PUT …/document`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ReviseCampaignQuery {
    /// The plan the caller read the body off; `409` if the campaign moved.
    #[serde(deserialize_with = "crate::parsed")]
    pub base_plan_id: PlanId,
    /// The revision's note, as on `POST /api/v1/campaigns/{campaign_id}/plans`.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<NonEmptyTrimmedString>,
    /// Expand the revision, and file nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// Let a cell the head has not answered take a finished run of the
    /// campaign's earlier plans, as it takes one of the head's. Files the head's
    /// own document too.
    #[serde(default)]
    pub carry_earlier: bool,
    /// Plans of any campaign, comma-separated, whose finished runs a cell the
    /// head has not answered may take, as it takes one of the head's. Files the
    /// head's own document too. At most 32, `add_from`'s and `restore`'s
    /// plans included.
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub inherit_from: Option<Vec<PlanId>>,
    /// A variant edit only: plans of any campaign, comma-separated, whose
    /// variants it appends after the body's, each with its plan's `benchmarks`
    /// where it names none. Their finished runs are offered as
    /// `inherit_from`'s.
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub add_from: Option<Vec<PlanId>>,
    /// A variant edit only: variants of plans of any campaign,
    /// comma-separated, each `<plan_id>:<index>`, numbered as
    /// `GET /api/v1/plans/{plan_id}/variants` lists them. They are appended
    /// after the body variants and the `add_from` variants, in the given
    /// order. The server offers finished runs from their plans through
    /// `inherit_from`.
    #[serde(
        default,
        with = "crate::comma_list",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub restore: Option<Vec<RestoredVariant>>,
}

/// The query string of `…/rerun` and `…/recalculate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RerunJobsQuery {
    /// The plan the caller read the jobs off; `409` if the campaign moved.
    /// Required with a whole-plan selector.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_plan_id: Option<PlanId>,
    /// Check the jobs and expand the document, and file nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// The query string of `…/revert`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RevertCampaignQuery {
    /// The plan the campaign runs, as the caller read it; `409` if the
    /// campaign moved.
    #[serde(deserialize_with = "crate::parsed")]
    pub base_plan_id: PlanId,
    /// Expand the revision, and file nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// What a campaign read adds: `GET /api/v1/campaigns` to its rows, and
/// `GET /api/v1/campaigns/{campaign_id}` to its campaign.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignReadQuery {
    /// The owners' names.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub names: Option<bool>,
    /// What each campaign's plan is doing.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub plan: Option<bool>,
}

/// The filter `GET /api/v1/campaigns` takes beside its owner filters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignListQuery {
    /// Narrow to the campaign this plan was filed into, current or superseded.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub plan_id: Option<PlanId>,
    /// Narrow to the campaigns in this state; every state without one.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub state: Option<CampaignState>,
}

/// What `GET /api/v1/campaigns/{campaign_id}/overview` adds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignOverviewQuery {
    /// The two device-time totals, one results query each.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub device_time: Option<bool>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[rstest::rstest]
    #[case::padded(json!({"note": "  nightly-2  "}), Some("nightly-2"))]
    #[case::blank(json!({"note": "   "}), None)]
    #[case::null(json!({"note": null}), None)]
    #[case::absent(json!({}), None)]
    fn every_request_reads_a_note_trimmed_and_blank_as_none(
        #[case] fields: Value,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let with = |key: &str, value: &str| {
            let mut body = fields.clone();
            if let Some(object) = body.as_object_mut() {
                object.insert(key.to_owned(), json!(value));
            }
            body
        };
        let notes = [
            serde_json::from_value::<RerunJobsRequest>(fields.clone())?.note,
            serde_json::from_value::<RecalculateJobsRequest>(fields.clone())?.note,
            serde_json::from_value::<RevertCampaignRequest>(with("plan_id", "plan-1"))?.note,
            serde_json::from_value::<CampaignPlanDocumentQuery>(fields.clone())?.note,
            serde_json::from_value::<ReviseCampaignQuery>(with("base_plan_id", "plan-1"))?.note,
            serde_json::from_value::<CreateCampaignQuery>(with("name", "nightly"))?.note,
        ];
        assert_eq!(
            notes
                .each_ref()
                .map(|note| note.as_ref().map(NonEmptyTrimmedString::as_str)),
            [expected; 6]
        );
        Ok(())
    }
}
