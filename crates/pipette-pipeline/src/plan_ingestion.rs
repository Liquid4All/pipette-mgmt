//! Plan ingestion: validate a set of job bodies as a unit, mint identities, and
//! stage them into the `todo/` queue gated by a plan manifest. The CLI and the
//! HTTP routes are thin transports over [`ingest_jobs`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize as _;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

use pipette_core::benchmark::Benchmark;
use pipette_core::client;
use pipette_core::matching;
use pipette_core::matching::{
    REQUIRED_FLAGS, REQUIRED_SUFFIX, capability_clauses_malformed, describe_clauses,
    job_matches_client,
};
use pipette_core::plan::{
    JobExpiry, PlanManifest, PlanNotes, PlanSource, SettledOutcome, StarvedGroup, past_gate,
};
use pipette_core::todo_filename::{parse_avail_filename, parse_leased_key};
use pipette_core::vault::VaultOwner;
use pipette_plan_types::{
    BenchmarkId, BenchmarkType, CampaignId, ClientRunSpec, EvalContextError, ExpiresAt,
    FleetDetail, GgufVision, GgufVisionSource, HeldJob, JobId, Model, ModelFlags, ModelSource,
    NonEmptyTrimmedString, PlanId, PlanStatus, RunSpecError, RuntimeType, Torch, UserId,
    VaultResourceId, VlmEvalRecipeError, Warning, WarningKind, unmatched_warning,
};
use pipette_store::{
    CellResult, CellScope, ClientStore, ContentKeyCas, ContentKeyClaim, ContentKeyRecord,
    ContentKeyVersion, CreateOutcome, OrgStore, PLAN_JOB_CONCURRENCY, PlanStore, Stores,
    SubmissionStore, TagStore, TodoStore, VaultStore, drain_clients, warn_unreadable_clients,
};

use crate::content_key::{content_digest, merge_duplicates, mergeable};
use crate::gated_import;
use crate::gated_import::without_nulls;
use crate::gated_import::{DryRunImports, Gated, GatedJob};
use crate::hf_import::HfImports;
use crate::model_params::{ModelParamTable, TypedMatch};
use crate::submission::without_auth_tokens;
use crate::vault_check;
use crate::vault_check::{VaultCheckError, VaultScope};
use crate::{plan_document, recalculation, vault_alias};

/// Ingestion's default expiry, so an omitted `expires_at` still bounds the
/// queue lifetime. A job another planner writes keeps the never-expires default.
const DEFAULT_EXPIRY_DAYS: i64 = 30;

/// Why an ingest failed; a transport maps each arm to a status. Typed, not an
/// `anyhow` leaf: a `match` is compiler-checked, and a `downcast_ref` breaks
/// once a call site in between re-wraps the error.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Rejected(#[from] RejectionError),
    /// The set needs an import, and every import permit of the process is
    /// held (`hf_import_max_concurrent`).
    #[error("too many HuggingFace imports are in progress; retry shortly")]
    ImportBusy,
    /// HuggingFace answered `429` for the model of the job at `index`. `reason`
    /// names the model, never its token, and the wait if HuggingFace sent one.
    #[error("could not import the model of job {label:?} (index {index}) at this time: {reason}")]
    HubRateLimited {
        index: usize,
        label: String,
        reason: String,
    },
    /// HuggingFace failed while it served the model of the job at `index`.
    /// `reason` names the model by its HuggingFace reference, never its token.
    #[error("could not import the model of job {label:?} (index {index}): {reason}")]
    Upstream {
        index: usize,
        label: String,
        reason: String,
    },
    /// Not a boxed error: `anyhow`'s `{:#}` prints the whole context chain, which
    /// `From<IngestError> for AppError` puts in the operator's log.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A job body or an import result that breaks what the import step relies on:
/// a bug, so the ingest fails with a `500`.
#[derive(Debug, thiserror::Error)]
pub enum ImportStepError {
    #[error("the import gave no vault arm for {model}")]
    NoVaultArm { model: String },
    #[error("job {label:?} lost its spec.model in the import step")]
    LostModel { label: String },
    #[error("the model for job {label:?} did not convert to JSON")]
    ModelJson {
        label: String,
        #[source]
        source: serde_json::Error,
    },
}

impl From<ImportStepError> for IngestError {
    fn from(e: ImportStepError) -> Self {
        Self::Other(anyhow::Error::new(e))
    }
}

/// A plan source already exists under the plan id that an ingest minted. The
/// id is fresh, so this is a bug, and the ingest fails with a `500`.
#[derive(Debug, thiserror::Error)]
#[error("a plan source already exists for the newly minted plan id {plan_id}")]
struct PlanSourceExistsError {
    plan_id: PlanId,
}

/// A validation rejection of a job set. Validation is fail-fast, so this names
/// the first offending job, and the set may hold others.
#[derive(Debug, thiserror::Error)]
pub enum RejectionError {
    /// The set carries no jobs, so there is no job for an index to name.
    #[error("plan declares no jobs")]
    NoJobs,
    /// The set carries more jobs than `plan_max_jobs` allows. The count is a
    /// fact about the whole set, so no index names a job here either.
    #[error("plan declares {count} jobs, over the ceiling of {limit} (`plan_max_jobs`)")]
    TooManyJobs { count: usize, limit: NonZeroUsize },
    /// A re-run's or a recalculation's document no longer places `cells` of
    /// the plan it revises, so filing it would run more than the named jobs.
    #[error(
        "the plan document no longer expands to the plan it revises: {cells} of that plan's \
         cells would run again or be dropped, not only the named ones"
    )]
    RerunDrift { cells: usize },
    /// `index` is what the API reports and `label` what the CLI reports, so no
    /// transport parses the message. `kind` is interpolated, not a `#[source]`,
    /// so one `to_string()` gives the whole rejection without repeats.
    #[error("rejected job {label:?} (index {index}): {kind}")]
    Job {
        index: usize,
        label: String,
        kind: JobRejectionError,
    },
}

impl RejectionError {
    /// The index of the job this rejection names. `None` for a rejection of
    /// the whole set.
    pub fn job_index(&self) -> Option<usize> {
        match self {
            Self::NoJobs | Self::TooManyJobs { .. } | Self::RerunDrift { .. } => None,
            Self::Job { index, .. } => Some(*index),
        }
    }
}

/// Two vlm_eval (or two asr_eval) entries for one task and dataset version
/// both cover the cell's model, so its score on the task would depend on which
/// a plan names. The catalog must say one.
#[derive(Debug, thiserror::Error)]
#[error("{repo} is covered by two recipes for {task} {dataset_version}: {benchmark} and {other}")]
pub struct AmbiguousRecipeError {
    pub repo: String,
    pub task: String,
    pub dataset_version: String,
    pub benchmark: BenchmarkId,
    pub other: BenchmarkId,
}

/// Which validation rejection class a job fell into. Each carries the offending
/// value, and no job identity: [`RejectionError::Job`] owns that.
#[derive(Debug, thiserror::Error)]
pub enum JobRejectionError {
    #[error("body is not a JSON object")]
    NotAnObject,
    #[error("carries a job_id (server-assigned at ingestion)")]
    PresetJobId,
    #[error("carries a plan_id (server-assigned at ingestion)")]
    PresetPlanId,
    #[error("declares neither a non-empty `requires` nor a non-empty `clients`")]
    NoEligibility,
    #[error(
        "a fail-closed eligibility clause is malformed (`requires`, `any_of`, `excludes`, \
         `not_all_of` and `exclude_clients` each hold string flags or ids, `any_of` and \
         `not_all_of` grouped one array deep)"
    )]
    MalformedCapabilityClauses,
    #[error("`clients` must be an array")]
    ClientsNotAnArray,
    #[error("`clients` must contain only strings")]
    ClientsNotStrings,
    /// No possible client can match the body
    /// ([`pipette_core::matching::unmatchable_reason`]). A body that matches
    /// nothing in today's fleet is a warning, not this.
    #[error("no possible client can match: {rule}")]
    StaticallyUnmatchable { rule: String },
    #[error(
        "more than one `{namespace}:` flag in a `not_all_of` group (no client can carry the whole group, so the group excludes nobody)"
    )]
    VacuousExclusionGroup { namespace: String },
    #[error("capability flag {flag:?} must be canonical (lowercase, no whitespace)")]
    NonCanonicalFlag { flag: String },
    #[error(
        "more than one `{namespace}:` flag in `requires` (reserved namespaces allow at most one)"
    )]
    DuplicateReservedNamespace { namespace: String },
    /// `value` is the offending JSON, so the message distinguishes the string
    /// `"soon"` from the number `42`.
    #[error("malformed `expires_at` {value} (expected an RFC 3339 string or \"never\")")]
    MalformedExpiresAt { value: String },
    #[error(
        "`expires_at` {value} is at or before the time of ingestion, so the job could never \
         be claimed; set a later expires_at, or omit it for the {DEFAULT_EXPIRY_DAYS}-day default"
    )]
    PastExpiresAt { value: String },
    #[error("missing required field `spec`")]
    MissingSpec,
    #[error("`spec` is not a JSON object")]
    SpecNotAnObject,
    #[error("`spec.benchmark` is missing or not a string")]
    MissingBenchmark,
    #[error("invalid `spec.benchmark`: {0}")]
    InvalidBenchmarkId(pipette_plan_types::BenchmarkIdError),
    #[error("spec.benchmark {id} not in the benchmark catalog{}", did_you_mean(.nearest.as_ref()))]
    UnknownBenchmark {
        id: BenchmarkId,
        /// The catalog name a typo of `id` most likely meant.
        nearest: Option<BenchmarkId>,
    },
    #[error("`spec` is missing required field `{field}`")]
    MissingSpecField { field: &'static str },
    /// Only for a spec this build parses.
    #[error("`spec` cannot run: {0}")]
    UnrunnableSpec(RunSpecError),
    /// Only for a spec this build parses, on an eval benchmark its cell cannot
    /// run as defined ([`EvalBenchmark::check_cell_context`]).
    ///
    /// [`EvalBenchmark::check_cell_context`]: pipette_plan_types::EvalBenchmark::check_cell_context
    #[error("`spec` cannot run {benchmark} as the catalog defines it: {reason}")]
    EvalContext {
        benchmark: BenchmarkId,
        reason: EvalContextError,
    },
    /// On a vlm_eval or asr_eval whose recipe does not cover the cell's model
    /// or thinking mode
    /// ([`VlmEvalRecipe::check`](pipette_plan_types::VlmEvalRecipe::check)).
    #[error("`spec` is off {benchmark}'s recipe: {reason}")]
    OffRecipe {
        benchmark: BenchmarkId,
        reason: Box<VlmEvalRecipeError>,
    },
    #[error(transparent)]
    AmbiguousRecipe(Box<AmbiguousRecipeError>),
    /// A vlm_eval or asr_eval on a repo's own (torch) weights runs on uv
    /// vLLM alone: on any other runtime it is a cell no client has.
    #[error(
        "`spec` runs {benchmark} on a torch model with runtime {runtime}; a vlm_eval or an \
         asr_eval runs a repo's own weights on uv_vllm alone"
    )]
    VlmEvalTorchRuntime {
        benchmark: BenchmarkId,
        runtime: String,
    },
    /// An asr_eval entry whose prompt profile or decoding its spec's runtime
    /// cannot run: on vLLM, a model it has no implementation of, or beams.
    #[error("`spec` runs {benchmark} on runtime {runtime}: {reason}")]
    AsrEvalRuntime {
        benchmark: BenchmarkId,
        runtime: String,
        reason: Box<pipette_plan_types::AsrDecodingError>,
    },
    /// A vlm_eval or asr_eval spec whose model or model flags this build
    /// cannot read: its recipe cannot be checked, so the job is not queued.
    #[error("`spec.{field}` cannot be checked against {benchmark}'s recipe: {reason}")]
    RecipeUnchecked {
        benchmark: BenchmarkId,
        field: &'static str,
        reason: String,
    },
    /// `value` is the offending JSON, so the message distinguishes the string
    /// `"false"` from the boolean `false`.
    #[error("malformed `merge_duplicates` {value} (expected a boolean)")]
    MalformedMergeDuplicates { value: String },
    #[error(
        "carries `{REQUIRED_FLAGS}` (ingestion writes it from the flags marked `{REQUIRED_SUFFIX}`)"
    )]
    PresetRequiredFlags,
    #[error(
        "{flag:?} in `{field}` is marked `{REQUIRED_SUFFIX}`; only a `requires` or `any_of` flag can be"
    )]
    RequiredOutsidePositives { field: &'static str, flag: String },
    #[error(
        "an `any_of` group marks some of its flags `{REQUIRED_SUFFIX}` and not others; mark all of them or none"
    )]
    PartlyRequiredGroup,
    #[error("{flag:?} names no tag or build to mark `{REQUIRED_SUFFIX}`")]
    BareRequiredSuffix { flag: String },
    /// Only under [`ModelCheck::strict`].
    #[error(
        "`spec.model` matches no entry in the model parameter table: {model}. Add a \
         model-params row for it, or drop --strict-models to ingest anyway and repair \
         the rows later with `fix-model-param`"
    )]
    UncataloguedModel { model: String },
    /// A `vault_*` source the server cannot check: no `principal_id`, or no URL
    /// this server serves (`crate::vault_check`).
    #[error("malformed vault source: {reason}")]
    VaultRefMalformed { reason: String },
    /// Also a resource outside the filer's [`VaultScope`], so the refusal
    /// names no owner.
    #[error(
        "vault resource {resource_id} does not exist on this server, or is outside the filer's reach"
    )]
    UnknownVaultResource { resource_id: VaultResourceId },
    /// The plan says the fetch is for `claimed`; the record says its bytes are
    /// `owner`'s. No device can satisfy both, so the job can only ever `403`.
    #[error(
        "vault resource {resource_id} belongs to {owner}, not to the principal_id {claimed} the \
         plan names"
    )]
    VaultPrincipalMismatch {
        resource_id: VaultResourceId,
        claimed: String,
        owner: String,
    },
    /// The record is there and its object is not: the producer removed it.
    #[error("vault resource {resource_id} names an object that is no longer there")]
    UnbackedVaultResource { resource_id: VaultResourceId },
    /// The plan pins one digest and the record another. Every device would
    /// download the artifact and then refuse to run on it.
    #[error(
        "vault resource {resource_id} is pinned to {pinned}, not the sha256 {claimed} the plan \
         names"
    )]
    VaultDigestMismatch {
        resource_id: VaultResourceId,
        claimed: String,
        pinned: String,
    },
    /// The plan calls the resource one thing and its record another. A label
    /// is what a reader takes the artifact for, so a wrong one is refused.
    #[error(
        "vault resource {resource_id} is named {name:?}, not the custom_label {claimed:?} the \
         plan names"
    )]
    VaultCustomLabelMismatch {
        resource_id: VaultResourceId,
        claimed: String,
        name: String,
    },
    /// An `auth_token` that the import does not remove (`crate::gated_import`).
    /// `field` is the JSON pointer of the token. The message never holds the
    /// value.
    #[error(
        "carries an `auth_token` at {field} that ingestion cannot import. Only a `spec.model` \
         that is a HuggingFace model of a known type, with no other fields, may carry a token"
    )]
    AuthToken { field: String },
    #[error(
        "carries an `auth_token`, and this ingest cannot import a model from HuggingFace. File \
         it into a campaign over the API"
    )]
    ImportUnavailable,
    /// The import of the job's model refused it. `reason` names the model by
    /// its HuggingFace reference, never its token.
    #[error("cannot import the model from HuggingFace: {reason}")]
    Import { reason: String },
}

fn did_you_mean(nearest: Option<&BenchmarkId>) -> String {
    nearest.map_or_else(String::new, |name| format!("; did you mean {name}?"))
}

/// The catalog name within a typo of `id`: an edit distance of at most a
/// quarter of its length, the nearest first and then by name.
fn nearest_benchmark(
    catalog: &HashMap<BenchmarkId, Benchmark>,
    id: &BenchmarkId,
) -> Option<BenchmarkId> {
    catalog
        .keys()
        .map(|name| (strsim::levenshtein(id.as_str(), name.as_str()), name))
        .filter(|(distance, _)| distance * 4 <= id.as_str().len())
        .min()
        .map(|(_, name)| name.clone())
}

/// How ingestion treats a job whose `spec.model` matches no model-params row.
/// A warning by default: an empty table is supported and a partner model has
/// no entry, and the row only lacks counts, which `fix-model-param` repairs.
/// Matching is the scorer's rule ([`ModelParamTable::resolve_typed_descriptor`]).
#[derive(Clone, Copy)]
pub struct ModelCheck<'a> {
    table: Option<&'a ModelParamTable>,
    strict: bool,
}

impl<'a> ModelCheck<'a> {
    /// No table to check against: warns about nothing, rejects nothing.
    pub fn none() -> Self {
        Self {
            table: None,
            strict: false,
        }
    }

    /// Warn on an uncatalogued model; ingest it anyway.
    pub fn warn(table: &'a ModelParamTable) -> Self {
        Self {
            table: Some(table),
            strict: false,
        }
    }

    /// Reject the whole plan if any job names an uncatalogued model. Fail-fast,
    /// so nothing is written.
    pub fn strict(table: &'a ModelParamTable) -> Self {
        Self {
            table: Some(table),
            strict: true,
        }
    }

    /// Whether this check reads a table with entries. Only then does the form
    /// in which the check reads a model matter.
    fn reads_table(&self) -> bool {
        self.table.is_some_and(|table| !table.is_empty())
    }

    /// Whether `model` resolves; an empty table, like none, resolves everything.
    /// Both [`TypedMatch`] failures count: either way the row lands without counts.
    fn resolves(&self, model: &Value) -> bool {
        match self.table {
            Some(table) if self.reads_table() => matches!(
                table.resolve_typed_descriptor(&model.to_string()),
                TypedMatch::Resolved { .. }
            ),
            _ => true,
        }
    }
}

/// The result of a successful ingest: the `campaigns ingest` stdout report.
/// `job_ids` maps each input label to the `job_id` it landed on, in input order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestReport {
    pub plan_id: PlanId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<pipette_plan_types::RevisionChange>,
    /// Distinct jobs this plan holds. Inputs that describe one cell share one
    /// job, so this can be smaller than the input count.
    pub job_count: usize,
    /// Inputs that merged onto a job instead of a new mint. This includes an
    /// earlier finished job that answers the cell. Overlaps `job_count`: an
    /// input that joins an earlier plan's job counts in both, one that joins a
    /// sibling of this plan counts here alone.
    pub merged_job_count: usize,
    #[serde(serialize_with = "serialize_jobs_map")]
    pub job_ids: Vec<(String, JobId)>,
    /// Per-input merge detail. CLI only: over HTTP, naming the cell tells the
    /// caller what another tenant already queued.
    pub merges: Vec<MergedInput>,
    pub warnings: Vec<Warning>,
    /// The planner's warnings on a plan document. Empty for job bodies.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plan_warnings: Vec<String>,
}

impl IngestReport {
    /// The report a reader at `detail` reads, without the CLI's `merges`. Not a
    /// redaction: a UUIDv7 job id still shows which inputs landed on older jobs.
    pub fn view(self, detail: FleetDetail) -> pipette_plan_types::PlanFiled {
        pipette_plan_types::PlanFiled {
            fleet_detail: FleetDetail::Full,
            plan_id: self.plan_id,
            note: self.note,
            change: self.change,
            job_count: self.job_count,
            merged_job_count: self.merged_job_count,
            job_ids: self.job_ids,
            warnings: self.warnings,
            plan_warnings: self.plan_warnings,
        }
        .at(detail)
    }
}

/// One input that merged onto an existing job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedInput {
    /// The input's label: its index in the expansion.
    pub label: String,
    pub job_id: JobId,
    pub kind: MergeKind,
}

/// Which duplicate a merge collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MergeKind {
    WithinPlan,
    AcrossPlans,
    Carried,
    /// An earlier finished job whose run answers the cell
    /// ([`satisfy_from_earlier_runs`]).
    Satisfied,
}

impl MergeKind {
    fn label(self) -> &'static str {
        self.into()
    }
}

/// Which campaign a plan is filed into, and what it supersedes there: one
/// value, because the two are only valid together. Both land on the manifest
/// and never move, so "one plan, one campaign" holds by construction.
#[derive(Debug, Clone)]
pub struct CampaignMembership {
    pub campaign_id: CampaignId,
    /// The campaign's head at the moment this plan was filed, which this plan
    /// now supersedes. `None` for the first plan in a campaign.
    pub supersedes: Option<PlanId>,
    /// The superseded plan's jobs to take over. Empty except for a variant
    /// edit, a re-run or a recalculation.
    pub carry: Carryover,
    /// The principal that owns the vault records of a HuggingFace import: the
    /// campaign's owner.
    pub owner: VaultOwner,
}

impl CampaignMembership {
    /// The first plan of a test campaign that `owner` owns. No campaign record
    /// backs the id, which ingestion never reads.
    #[cfg(test)]
    pub(crate) fn for_test(owner: UserId) -> Self {
        Self {
            campaign_id: <CampaignId as pipette_core::types::Mint>::mint(),
            supersedes: None,
            carry: Carryover::default(),
            owner: VaultOwner::User { user_id: owner },
        }
    }

    /// A plan filed into `campaign`, superseding its head. Takes the campaign,
    /// not its id, so the superseded plan cannot come from anywhere else.
    pub fn of(campaign: &pipette_core::campaign::Campaign) -> Self {
        Self {
            campaign_id: campaign.campaign_id.clone(),
            supersedes: campaign.current_plan_id.clone(),
            carry: Carryover::default(),
            owner: campaign.owner.clone(),
        }
    }

    /// A variant edit, re-run or recalculation of `campaign`: [`of`](Self::of),
    /// taking over the jobs of `previous` (the head, which the caller checks)
    /// except the `rerun` and `recalculate` ones.
    pub(crate) fn carrying(
        campaign: &pipette_core::campaign::Campaign,
        previous: &PlanManifest,
        rerun: &BTreeSet<JobId>,
        recalculate: &BTreeSet<JobId>,
    ) -> Self {
        Self {
            carry: Carryover::from_manifest(previous, rerun, recalculate),
            ..Self::of(campaign)
        }
    }

    /// The first plan of `campaign`, which is being created with it and so
    /// supersedes nothing.
    pub fn first(campaign: &pipette_core::campaign::NewCampaign) -> Self {
        Self {
            campaign_id: campaign.campaign_id().clone(),
            supersedes: None,
            carry: Carryover::default(),
            owner: campaign.owner().clone(),
        }
    }
}

/// The superseded plan's jobs by content digest, which a campaign revision
/// takes over finished or not: the content-key merge joins live jobs only.
#[derive(Debug, Clone, Default)]
pub struct Carryover {
    pool: HashMap<String, VecDeque<JobId>>,
    /// The jobs a re-run withheld from `pool`, by digest.
    rerun: HashMap<String, RerunCell>,
    /// The jobs a recalculation withheld from `pool`, by digest, that no
    /// placement has drawn yet.
    recalculate: HashMap<String, VecDeque<JobId>>,
    /// The superseded plan's [`PlanManifest::rerun_of`], which a job it
    /// carries keeps.
    rerun_of: BTreeMap<JobId, JobId>,
    /// The body each job of `pool` was filed under, for a cell whose clauses
    /// changed ([`Unclaimed`]). Empty where the job sources were not read.
    sources: HashMap<JobId, Value>,
    /// Other plans' jobs on offer, in the order to take them
    /// ([`Self::with_offered`]).
    offered: Vec<Offered>,
}

/// A job of a plan other than the superseded one, offered to a revision's
/// cells, with the body it was filed under.
#[derive(Debug, Clone)]
pub struct Offered {
    pub plan_id: PlanId,
    pub job_id: JobId,
    pub body: Value,
}

/// One digest's jobs a re-run withheld, and whether a placement has drawn the
/// digest yet.
#[derive(Debug, Clone, Default)]
struct RerunCell {
    /// In job id order: opted-out twins share a digest, so which placement
    /// names which twin is arbitrary.
    jobs: VecDeque<JobId>,
    drawn: bool,
}

/// What one placement takes from the plan a campaign revision supersedes.
#[derive(Debug)]
enum Inherited {
    Nothing,
    /// That plan's job for the cell.
    Job(JobId),
    /// A cell a re-run named, with none of that plan's jobs left for it, and
    /// the job it runs again, `None` once that plan's runs of the cell are
    /// used up.
    Rerun(Option<JobId>),
    /// A cell a recalculation named: a new job, answered from the record of
    /// that plan's job.
    Recalculate(JobId),
    /// That plan's finished job for the cell under other clauses, whose run
    /// still answers this one's ([`Unclaimed::answer`]).
    Answered(JobId),
    /// Any plan's finished job for the cell, whose run answers this one's
    /// ([`satisfy_from_earlier_runs`]).
    Satisfied(JobId),
}

impl Carryover {
    /// Every job `previous` recorded a digest for, less the ones that lapsed
    /// for it, which never answered it, and the `rerun` and `recalculate`
    /// ones: the cells of all three are placed afresh.
    fn from_manifest(
        previous: &PlanManifest,
        rerun: &BTreeSet<JobId>,
        recalculate: &BTreeSet<JobId>,
    ) -> Self {
        let carry = Self {
            rerun_of: previous.rerun_of.clone(),
            ..Self::default()
        };
        previous
            .content_keys
            .iter()
            .fold(carry, |mut carry, (job_id, digest)| {
                if rerun.contains(job_id) {
                    carry
                        .rerun
                        .entry(digest.clone())
                        .or_default()
                        .jobs
                        .push_back(job_id.clone());
                } else if recalculate.contains(job_id) {
                    carry
                        .recalculate
                        .entry(digest.clone())
                        .or_default()
                        .push_back(job_id.clone());
                } else if !previous.expired_jobs.contains_key(job_id) {
                    carry
                        .pool
                        .entry(digest.clone())
                        .or_default()
                        .push_back(job_id.clone());
                }
                carry
            })
    }

    /// What one placement of `digest` inherits. A plan that opted out of
    /// merging may have run the cell more than once.
    fn take(&mut self, digest: &str) -> Inherited {
        if let Some(job_id) = self.pool.get_mut(digest).and_then(VecDeque::pop_front) {
            return Inherited::Job(job_id);
        }
        if let Some(job_id) = self
            .recalculate
            .get_mut(digest)
            .and_then(VecDeque::pop_front)
        {
            return Inherited::Recalculate(job_id);
        }
        match self.rerun.get_mut(digest) {
            Some(cell) => {
                cell.drawn = true;
                Inherited::Rerun(cell.jobs.pop_front())
            }
            None => Inherited::Nothing,
        }
    }

    /// The body each job was filed under, which [`Unclaimed`] reads.
    pub(crate) fn with_sources(self, sources: HashMap<JobId, Value>) -> Self {
        Self { sources, ..self }
    }

    /// Offer `offered`, jobs of plans other than the superseded one, to the
    /// cells that would otherwise run.
    pub(crate) fn with_offered(self, offered: Vec<Offered>) -> Self {
        Self { offered, ..self }
    }

    /// One draw per unit, refused when a re-run or a recalculation leaves a
    /// cell of its plan behind.
    fn draw(mut self, units: &[Unit]) -> Result<Draw, RejectionError> {
        let inherited = units.iter().map(|unit| self.take(&unit.digest)).collect();
        self.require_named_placed()?;
        Ok(Draw {
            inherited,
            rerun_of: self.rerun_of,
            unclaimed: Unclaimed::of(self.pool, self.sources, self.offered),
        })
    }

    /// Refuse a re-run or a recalculation whose draw left a cell of the plan it
    /// revises behind: its unchanged document now expands differently.
    fn require_named_placed(&self) -> Result<(), RejectionError> {
        if self.rerun.is_empty() && self.recalculate.is_empty() {
            return Ok(());
        }
        let left = self.pool.values().map(VecDeque::len).sum::<usize>()
            + self.recalculate.values().map(VecDeque::len).sum::<usize>()
            + self.rerun.values().filter(|cell| !cell.drawn).count();
        match left {
            0 => Ok(()),
            cells => Err(RejectionError::RerunDrift { cells }),
        }
    }
}

/// What [`Carryover::draw`] gives the units, one draw each, with the
/// superseded plan's `rerun_of` and the jobs no unit drew.
struct Draw {
    inherited: Vec<Inherited>,
    rerun_of: BTreeMap<JobId, JobId>,
    unclaimed: Unclaimed,
}

/// The jobs a unit may take by [`crate::content_key::cell_digest`]: the
/// superseded plan's that no unit drew, oldest first, then the other plans'
/// ([`Carryover::with_offered`]), in the order offered.
#[derive(Debug)]
struct Unclaimed {
    cells: HashMap<String, Vec<Candidate>>,
    /// Whether other plans' jobs are on offer, which lets one stand in for a
    /// queued job of the superseded plan.
    offers: bool,
}

#[derive(Debug)]
struct Candidate {
    job_id: JobId,
    body: Value,
    /// The plan that offered it, `None` for the superseded one.
    from: Option<PlanId>,
}

/// What a job's submission record says of its run.
struct Run {
    succeeded: bool,
    /// [`client::recorded_run_flags`] of the record.
    flags: BTreeSet<String>,
}

/// What [`Unclaimed::answer`] gave the units beyond their draws.
#[derive(Debug, Default)]
struct Answers {
    /// The indices of the units that drew nothing, had a recorded run of their
    /// cell by the superseded plan, and took none.
    requirements_rerun: HashSet<usize>,
    /// The other plans' jobs the units took, with the plan each came from.
    offered: HashMap<JobId, PlanId>,
}

impl Unclaimed {
    fn of(
        pool: HashMap<String, VecDeque<JobId>>,
        mut sources: HashMap<JobId, Value>,
        offered: Vec<Offered>,
    ) -> Self {
        let mut cells: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut offer = |job_id: JobId, body: Value, from: Option<PlanId>| {
            cells
                .entry(crate::content_key::cell_digest(&body))
                .or_default()
                .push(Candidate { job_id, body, from });
        };
        let mut head: Vec<(JobId, Value)> = pool
            .into_values()
            .flatten()
            .filter_map(|job_id| sources.remove(&job_id).map(|body| (job_id, body)))
            .collect();
        head.sort_by(|(a, _), (b, _)| a.cmp(b));
        head.into_iter()
            .for_each(|(job_id, body)| offer(job_id, body, None));
        let offers = !offered.is_empty();
        offered.into_iter().for_each(
            |Offered {
                 plan_id,
                 job_id,
                 body,
             }| offer(job_id, body, Some(plan_id)),
        );
        Self { cells, offers }
    }

    /// Give each unit that drew nothing a finished job of its cell whose run
    /// still answers it ([`matching::run_answers`]). With other plans on
    /// offer, a unit that drew a queued job takes one in its place.
    async fn answer<'u>(
        mut self,
        units: &'u [Unit],
        body_of: impl Fn(&'u Unit) -> Option<&'u Value>,
        inherited: &mut [Inherited],
        submissions: &dyn SubmissionStore,
    ) -> anyhow::Result<Answers> {
        let queued: HashSet<JobId> = if self.offers {
            let drawn = drawn_jobs(inherited.iter());
            let settled = submissions.find_outcomes(&drawn).await?;
            drawn
                .into_iter()
                .filter(|job_id| !settled.contains_key(job_id))
                .collect()
        } else {
            HashSet::new()
        };
        let wanted: Vec<(usize, &Value, String, bool)> = units
            .iter()
            .enumerate()
            .filter_map(|(index, unit)| {
                let drew_nothing = match inherited.get(index)? {
                    Inherited::Nothing => true,
                    Inherited::Job(job_id) if queued.contains(job_id) => false,
                    Inherited::Job(_)
                    | Inherited::Rerun(_)
                    | Inherited::Recalculate(_)
                    | Inherited::Answered(_)
                    | Inherited::Satisfied(_) => return None,
                };
                let body = body_of(unit)?;
                let cell = crate::content_key::cell_digest(body);
                self.cells
                    .contains_key(&cell)
                    .then_some((index, body, cell, drew_nothing))
            })
            .collect();
        let cells: HashSet<&String> = wanted.iter().map(|(_, _, cell, _)| cell).collect();
        // Owned ids: a stream of borrowed ones trips higher-ranked `Send`
        // inference in the handlers that await this.
        let job_ids: Vec<JobId> = cells
            .into_iter()
            .filter_map(|cell| self.cells.get(cell))
            .flatten()
            .map(|candidate| candidate.job_id.clone())
            .collect();
        let runs: HashMap<JobId, Run> = futures::stream::iter(job_ids)
            .map(|job_id| async move {
                let record = submissions.find_job(&job_id).await?;
                anyhow::Ok(record.map(|record| {
                    let message_type = record.body.get("message_type").and_then(Value::as_str);
                    let run = Run {
                        succeeded: SettledOutcome::from_message_type(message_type)
                            == SettledOutcome::Finished,
                        flags: client::recorded_run_flags(&record.body),
                    };
                    (job_id, run)
                }))
            })
            .buffer_unordered(PLAN_JOB_CONCURRENCY)
            .try_filter_map(|run| async move { Ok(run) })
            .try_collect()
            .await?;
        let answers = wanted.into_iter().fold(
            Answers::default(),
            |mut answers, (index, body, cell, drew_nothing)| {
                let Some(candidates) = self.cells.get_mut(&cell) else {
                    return answers;
                };
                let answering = candidates.iter().position(|candidate| {
                    runs.get(&candidate.job_id).is_some_and(|run| {
                        run.succeeded && matching::run_answers(&candidate.body, body, &run.flags)
                    })
                });
                let Some(at) = answering else {
                    // The superseded plan's own run: another plan's is not it.
                    let ran = candidates.iter().any(|candidate| {
                        candidate.from.is_none() && runs.contains_key(&candidate.job_id)
                    });
                    if drew_nothing && ran {
                        answers.requirements_rerun.insert(index);
                    }
                    return answers;
                };
                let taken = candidates.remove(at);
                if let Some(plan_id) = taken.from {
                    answers.offered.insert(taken.job_id.clone(), plan_id);
                }
                if let Some(slot) = inherited.get_mut(index) {
                    *slot = Inherited::Answered(taken.job_id);
                }
                answers
            },
        );
        Ok(answers)
    }
}

/// The jobs of the superseded plan that `slots` drew, which may still be
/// queued.
fn drawn_jobs<'a>(slots: impl Iterator<Item = &'a Inherited>) -> Vec<JobId> {
    slots
        .filter_map(|slot| match slot {
            Inherited::Job(job_id) => Some(job_id.clone()),
            Inherited::Nothing
            | Inherited::Rerun(_)
            | Inherited::Recalculate(_)
            | Inherited::Answered(_)
            | Inherited::Satisfied(_) => None,
        })
        .collect()
}

/// The [`CellScope`] of a campaign of `owner`: the jobs it mints carry it, and
/// it reuses only the runs of jobs that carry it. A user in an organization
/// shares the scope of that organization. A failed read of the membership
/// stops the ingest, because a wrong scope would be stored with each job.
async fn cell_scope(owner: &VaultOwner, orgs: &dyn OrgStore) -> anyhow::Result<CellScope> {
    Ok(match owner {
        VaultOwner::Org { org_id } => CellScope::Org(org_id.clone()),
        VaultOwner::User { user_id } => match orgs
            .get_membership(user_id)
            .await
            .with_context(|| format!("reading the organization of {user_id}"))?
        {
            Some(org_id) => CellScope::Org(org_id),
            None => CellScope::User(user_id.clone()),
        },
    })
}

/// A unit that [`satisfy_from_earlier_runs`] gave an earlier finished job.
struct Satisfaction {
    /// The index of the unit.
    unit: usize,
    /// The [`crate::content_key::cell_digest`] of the unit.
    cell: String,
    /// The earlier finished job.
    job_id: JobId,
    /// The queued job of the superseded plan that the unit drew, if any.
    carried: Option<JobId>,
}

/// Give a unit an earlier finished job of its cell if the run of that job
/// answers the unit ([`PlanStore::find_cell_results`],
/// [`matching::run_answers`]). Only a job that `scope` minted is offered
/// ([`cell_scope`]). The pass covers each unit that would mint, and
/// each unit that would carry a queued job of the superseded plan. It takes the
/// newest such run. Each run answers at most one unit. A unit that opted out
/// keeps what it drew. A carried job that has a record, finished or failed,
/// also stays, because it is the run of the campaign.
async fn satisfy_from_earlier_runs<'u>(
    units: &'u [Unit],
    body_of: impl Fn(&'u Unit) -> Option<&'u Value>,
    inherited: &mut [Inherited],
    plans: &dyn PlanStore,
    submissions: &dyn SubmissionStore,
    scope: &CellScope,
) -> anyhow::Result<Vec<Satisfaction>> {
    let eligible: Vec<(usize, &Value, String)> = units
        .iter()
        .enumerate()
        .filter_map(|(index, unit)| {
            let body = body_of(unit)?;
            let open = matches!(
                inherited.get(index)?,
                Inherited::Nothing | Inherited::Job(_)
            );
            (open && mergeable(body)).then(|| (index, body, crate::content_key::cell_digest(body)))
        })
        .collect();
    if eligible.is_empty() {
        return Ok(Vec::new());
    }
    let cells: Vec<String> = eligible
        .iter()
        .map(|(_, _, cell)| cell.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let results = plans.find_cell_results(&cells, scope).await?;
    // Newest first within each cell, as the store returns them.
    let by_cell: HashMap<&str, Vec<&CellResult>> =
        results.iter().fold(HashMap::new(), |mut by_cell, result| {
            by_cell
                .entry(result.cell_digest.as_str())
                .or_default()
                .push(result);
            by_cell
        });
    let drawn = drawn_jobs(
        eligible
            .iter()
            .filter(|(_, _, cell)| by_cell.contains_key(cell.as_str()))
            .filter_map(|(index, ..)| inherited.get(*index)),
    );
    let recorded = if drawn.is_empty() {
        HashMap::new()
    } else {
        submissions.find_outcomes(&drawn).await?
    };
    // A job this plan already holds answers no other unit.
    let held: HashSet<JobId> = inherited
        .iter()
        .filter_map(|slot| match slot {
            Inherited::Job(job_id)
            | Inherited::Answered(job_id)
            | Inherited::Recalculate(job_id)
            | Inherited::Satisfied(job_id) => Some(job_id.clone()),
            Inherited::Rerun(job_id) => job_id.clone(),
            Inherited::Nothing => None,
        })
        .collect();
    let (_, satisfied) = eligible.into_iter().fold(
        (held, Vec::new()),
        |(mut held, mut satisfied), (index, body, cell)| {
            let carried = match inherited.get(index) {
                Some(Inherited::Job(job_id)) if recorded.contains_key(job_id) => {
                    return (held, satisfied);
                }
                Some(Inherited::Job(job_id)) => Some(job_id.clone()),
                _ => None,
            };
            let answering = by_cell.get(cell.as_str()).and_then(|runs| {
                runs.iter().find(|run| {
                    !held.contains(&run.job_id)
                        && matching::run_answers(&run.body, body, &run.run_flags)
                })
            });
            let Some(run) = answering else {
                return (held, satisfied);
            };
            held.insert(run.job_id.clone());
            if let Some(slot) = inherited.get_mut(index) {
                *slot = Inherited::Satisfied(run.job_id.clone());
            }
            satisfied.push(Satisfaction {
                unit: index,
                cell,
                job_id: run.job_id.clone(),
                carried,
            });
            (held, satisfied)
        },
    );
    Ok(satisfied)
}

/// What ingestion reads and writes beyond the plan itself: the stores, the two
/// catalogs, and the limits the server sets. One per transport.
#[derive(Clone, Copy)]
pub struct IngestContext<'a> {
    pub plans: &'a dyn PlanStore,
    pub todo: &'a dyn TodoStore,
    /// The roster that the fleet-match pass reads.
    pub clients: &'a dyn ClientStore,
    /// The roster's tags, which the fleet-match pass reads beside it.
    pub tags: &'a dyn TagStore,
    pub submissions: &'a dyn SubmissionStore,
    /// The organization of a campaign owner, which scopes the reuse of a
    /// finished run ([`cell_scope`]).
    pub orgs: &'a dyn OrgStore,
    pub vault: &'a dyn VaultStore,
    /// Whose vault resources the jobs may name.
    pub vault_scope: &'a VaultScope,
    /// The HuggingFace import of a model that carries a token. `None` for a
    /// transport that cannot import, which then refuses a body with a token.
    pub import: Option<&'a HfImports>,
    /// The benchmark catalog that each job's `benchmark_id` must name.
    pub catalog: &'a HashMap<BenchmarkId, Benchmark>,
    pub models: ModelCheck<'a>,
    /// The most jobs one plan may declare (`plan_max_jobs`).
    pub max_jobs: NonZeroUsize,
    /// The age past which `queue-maintenance` counts a `creating` manifest as
    /// stranded (`plan_creating_max_age_secs`). The merge rules read it.
    pub creating_max_age: std::time::Duration,
}

impl<'a> IngestContext<'a> {
    /// The context over a full set of stores, as the CLI transports hold them,
    /// with no import and every vault resource in reach: the host is past every
    /// gate. A transport that imports sets [`Self::import`].
    pub fn from_stores(
        stores: &'a Stores,
        catalog: &'a HashMap<BenchmarkId, Benchmark>,
        models: ModelCheck<'a>,
        max_jobs: NonZeroUsize,
        creating_max_age: std::time::Duration,
    ) -> Self {
        Self {
            plans: stores.plans.as_ref(),
            todo: stores.todo.as_ref(),
            clients: stores.clients.as_ref(),
            tags: stores.tags.as_ref(),
            submissions: stores.submissions.as_ref(),
            orgs: stores.orgs.as_ref(),
            vault: stores.vault.as_ref(),
            vault_scope: &VaultScope::Every,
            import: None,
            catalog,
            models,
            max_jobs,
            creating_max_age,
        }
    }

    /// The context over a full set of stores, as [`Self::from_stores`] builds
    /// it, with the model check, the limits, and the import of `settings`.
    pub fn with_settings(
        stores: &'a Stores,
        catalog: &'a HashMap<BenchmarkId, Benchmark>,
        settings: IngestSettings<'a>,
    ) -> Self {
        let IngestSettings {
            models,
            max_jobs,
            creating_max_age,
            import,
        } = settings;
        Self {
            import,
            ..Self::from_stores(stores, catalog, models, max_jobs, creating_max_age)
        }
    }
}

/// The parts of an [`IngestContext`] that a CLI command sets apart from its
/// stores and its benchmark catalog, built once and passed to each ingest.
#[derive(Clone, Copy)]
pub struct IngestSettings<'a> {
    pub models: ModelCheck<'a>,
    pub max_jobs: NonZeroUsize,
    pub creating_max_age: std::time::Duration,
    pub import: Option<&'a HfImports>,
}

/// One plan as a transport submits it: its inputs and the facts about who filed
/// it and where. `J` is the job bodies as they arrive, or [`PreparedJobs`]
/// after [`prepare_jobs`].
#[derive(Debug)]
pub struct PlanSubmission<J = Vec<(String, Value)>> {
    pub note: Option<NonEmptyTrimmedString>,
    /// What a campaign revision changed, as the server wrote it.
    pub change: Option<pipette_plan_types::RevisionChange>,
    /// Who the transport attributes the plan to, written onto the manifest as-is.
    pub submitted_by: Option<UserId>,
    pub membership: CampaignMembership,
    /// `(label, body)` pairs. The caller owns the labels, which are cardinal
    /// indices.
    pub jobs: J,
    /// The plan document the jobs were expanded from, stored beside the
    /// manifest. `None` files a plan with no document, the shape of a plan
    /// filed as job bodies. Only tests file one.
    pub source: Option<PlanSource>,
    /// The planner's warnings on [`source`](Self::source), stored on the
    /// manifest. Empty for a plan submitted as job bodies.
    pub plan_warnings: Vec<String>,
}

/// Run every validation check on a job set, and write nothing. [`prepare_jobs`]
/// and [`dry_run_jobs`] both run it, so a dry run and a real submission accept
/// the same job sets.
pub async fn validate_submission(
    ctx: &IngestContext<'_>,
    jobs: &[(String, Value)],
    now: DateTime<Utc>,
) -> Result<(), IngestError> {
    validate_into(ctx, jobs, now, &mut HashMap::new())
        .await
        .map(|_| ())
}

/// Run [`validate_submission`], and keep each vault record that it reads in
/// `seen`. Return the [`model_aliases`] entry of each job.
async fn validate_into(
    ctx: &IngestContext<'_>,
    jobs: &[(String, Value)],
    now: DateTime<Utc>,
    seen: &mut HashMap<VaultResourceId, Option<vault_check::Seen>>,
) -> Result<Vec<Option<Value>>, IngestError> {
    let IngestContext {
        vault,
        vault_scope,
        catalog,
        models,
        max_jobs,
        ..
    } = *ctx;
    // An empty set could never become `active` nor `complete`, so it would
    // linger for good.
    if jobs.is_empty() {
        return Err(RejectionError::NoJobs.into());
    }
    if jobs.len() > max_jobs.get() {
        return Err(RejectionError::TooManyJobs {
            count: jobs.len(),
            limit: max_jobs,
        }
        .into());
    }
    // Read within the filer's reach: a record outside it gives no origin, so a
    // refusal never names what another tenant's model was imported from.
    let aliases = model_aliases(vault, vault_scope, models, catalog, jobs).await?;
    jobs.iter()
        .zip(&aliases)
        .enumerate()
        .try_for_each(|(index, ((label, body), alias))| {
            validate_job(body, alias.as_ref(), catalog, models, now).map_err(|kind| {
                RejectionError::Job {
                    index,
                    label: label.clone(),
                    kind,
                }
            })
        })?;

    // A vault reference is judged against the record it names: a job whose
    // artifact no device can fetch can only fail.
    check_vault_refs(
        vault,
        vault_scope,
        jobs.iter()
            .enumerate()
            .map(|(index, (label, body))| (index, label, body)),
        seen,
    )
    .await?;
    Ok(aliases)
}

/// The HuggingFace form of each job's vault model, for the table check and a
/// vlm_eval's recipe: a re-expanded document names an imported model by its
/// vault arm. `None` for a model with no HuggingFace origin, and for all when
/// neither the model parameter table nor a recipe is read.
async fn model_aliases(
    vault: &dyn VaultStore,
    scope: &VaultScope,
    models: ModelCheck<'_>,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    jobs: &[(String, Value)],
) -> anyhow::Result<Vec<Option<Value>>> {
    let recipes = jobs
        .iter()
        .any(|(_, body)| recipe_of(catalog, body).is_some());
    if !models.reads_table() && !recipes {
        return Ok(vec![None; jobs.len()]);
    }
    let mut known: HashMap<String, Option<Value>> = HashMap::new();
    let mut aliases = Vec::with_capacity(jobs.len());
    // A loop, not an iterator chain: each new descriptor awaits a vault read
    // and fills `known`.
    for (_, body) in jobs {
        let Some(model) = body.get("spec").and_then(|spec| spec.get("model")) else {
            aliases.push(None);
            continue;
        };
        let descriptor = model.to_string();
        let alias = match known.get(&descriptor) {
            Some(alias) => alias.clone(),
            None => {
                let alias = vault_alias::model_alias_within(vault, Some(&descriptor), scope)
                    .await
                    .context("failed to read the origin of a vault model")?
                    .map(|alias| serde_json::from_str::<Value>(&alias))
                    .transpose()?;
                known.insert(descriptor, alias.clone());
                alias
            }
        };
        aliases.push(alias);
    }
    Ok(aliases)
}

/// Run [`vault_check::check`] on each body, and name the first job it refuses
/// by the index that the iterator gives.
async fn check_vault_refs<'b>(
    vault: &dyn VaultStore,
    scope: &VaultScope,
    jobs: impl Iterator<Item = (usize, &'b String, &'b Value)>,
    seen: &mut HashMap<VaultResourceId, Option<vault_check::Seen>>,
) -> Result<(), IngestError> {
    for (index, label, body) in jobs {
        vault_check::check(vault, scope, body, seen)
            .await
            .map_err(|e| match e {
                VaultCheckError::Rejected(kind) => IngestError::Rejected(RejectionError::Job {
                    index,
                    label: label.clone(),
                    kind,
                }),
                VaultCheckError::Store(e) => IngestError::Other(
                    e.context("failed to read a vault resource during ingestion"),
                ),
            })?;
    }
    Ok(())
}

/// A job set that passed every validation check and the import step. Only
/// [`prepare_jobs`] builds one, so [`ingest_prepared`] stages no body that
/// skipped the import.
#[derive(Debug)]
pub struct PreparedJobs {
    jobs: Vec<GatedJob>,
    /// Each model the import step imported, without its token, mapped to its
    /// vault arm, for the plan document [`ingest_prepared`] writes.
    rewrites: HashMap<Model, Model>,
}

/// Validate a job set, import each model that carries a token for `owner`, and
/// check the rewritten bodies against the vault. This writes vault records and
/// nothing else.
pub async fn prepare_jobs(
    ctx: &IngestContext<'_>,
    owner: &VaultOwner,
    mut jobs: Vec<(String, Value)>,
) -> Result<PreparedJobs, IngestError> {
    let now = Utc::now();
    lift_required(&mut jobs)?;
    let mut seen = HashMap::new();
    let aliases = validate_into(ctx, &jobs, now, &mut seen).await?;
    let Gated { mut jobs, rewrites } =
        gated_import::import(ctx.import, ctx.vault, owner, jobs, now).await?;
    check_rewritten(ctx, &jobs, &mut seen).await?;
    // A vault model that the import did not write reads as its HuggingFace
    // form in the coverage warnings, as it did in validation.
    jobs.iter_mut()
        .zip(aliases)
        .filter(|(job, _)| job.hf_model.is_none())
        .for_each(|(job, alias)| job.hf_model = alias);
    Ok(PreparedJobs { jobs, rewrites })
}

/// Run the checks of [`prepare_jobs`] on a job set, and write nothing: the
/// import resolves each model with a token against the Hub and downloads
/// nothing. Answers each job body without its `auth_token`, in job order.
pub(crate) async fn dry_run_jobs(
    ctx: &IngestContext<'_>,
    owner: &VaultOwner,
    mut jobs: Vec<(String, Value)>,
    now: DateTime<Utc>,
) -> Result<(Vec<Value>, DryRunImports), IngestError> {
    lift_required(&mut jobs)?;
    let mut seen = HashMap::new();
    validate_into(ctx, &jobs, now, &mut seen).await?;
    let bodies = jobs
        .iter()
        .map(|(_, body)| without_auth_tokens(body))
        .collect();
    let dry_run = gated_import::dry_run(ctx.import, ctx.vault, owner, jobs).await?;
    check_rewritten(ctx, &dry_run.jobs, &mut seen).await?;
    Ok((bodies, dry_run))
}

/// Run [`check_vault_refs`] on each job that the import step rewrote to a
/// vault arm. [`validate_into`] checked each other body, and `seen` holds the
/// records that it read.
async fn check_rewritten(
    ctx: &IngestContext<'_>,
    jobs: &[GatedJob],
    seen: &mut HashMap<VaultResourceId, Option<vault_check::Seen>>,
) -> Result<(), IngestError> {
    check_vault_refs(
        ctx.vault,
        ctx.vault_scope,
        jobs.iter()
            .enumerate()
            .filter(|(_, job)| job.hf_model.is_some())
            .map(|(index, job)| (index, &job.label, &job.body)),
        seen,
    )
    .await
}

/// Validate, import, mint, and stage a job set as one plan. A failure stages no
/// plan; the vault records of a completed import stay.
pub async fn ingest_jobs(
    ctx: &IngestContext<'_>,
    submission: PlanSubmission,
) -> Result<IngestReport, IngestError> {
    let PlanSubmission {
        note,
        change,
        submitted_by,
        membership,
        jobs,
        source,
        plan_warnings,
    } = submission;
    let jobs = prepare_jobs(ctx, &membership.owner, jobs).await?;
    ingest_prepared(
        ctx,
        PlanSubmission {
            note,
            change,
            submitted_by,
            membership,
            jobs,
            source,
            plan_warnings,
        },
    )
    .await
}

/// Mint and stage a job set that [`prepare_jobs`] accepted, as one plan.
///
/// A failure during staging rolls nothing back: a retry mints fresh ids, and
/// `queue-maintenance` tears down a manifest stranded in `creating`.
pub async fn ingest_prepared(
    ctx: &IngestContext<'_>,
    submission: PlanSubmission<PreparedJobs>,
) -> Result<IngestReport, IngestError> {
    let IngestContext {
        plans,
        todo,
        clients: auth,
        tags,
        submissions,
        orgs,
        models,
        creating_max_age,
        catalog,
        ..
    } = *ctx;
    let PlanSubmission {
        note,
        change,
        submitted_by,
        membership,
        jobs,
        source,
        plan_warnings,
    } = submission;
    // An import can run for hours, so staging reads the clock again and refuses
    // an `expires_at` that passed during the import.
    let PreparedJobs { jobs, rewrites } = jobs;
    let now = Utc::now();
    jobs.iter().enumerate().try_for_each(|(index, job)| {
        match job.body.get("expires_at").and_then(Value::as_str) {
            Some(s) if matches!(parse_expires_at(s), Some(ExpiresAt::At(at)) if at <= now) => {
                Err(RejectionError::Job {
                    index,
                    label: job.label.clone(),
                    kind: JobRejectionError::PastExpiresAt {
                        value: bounded(s.to_owned()),
                    },
                })
            }
            _ => Ok(()),
        }
    })?;

    // Every later parse of the document types its cells by these, not by the
    // catalog of the day. `validate_job` put each id in `catalog`.
    let benchmark_types = jobs
        .iter()
        .map(|job| {
            let id = job
                .body
                .pointer("/spec/benchmark")
                .and_then(Value::as_str)
                .and_then(|id| BenchmarkId::try_new(id).ok())
                .context("a prepared job names no benchmark")?;
            let benchmark = catalog
                .get(&id)
                .context("a prepared job's benchmark left the catalog")?;
            anyhow::Ok((id, benchmark.def.benchmark_type()))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

    // Before the first write, so a failure writes nothing. The submitted text's
    // digest recognizes the document when the client sends it again.
    let submitted = source.as_ref().map(|source| source.sha256().clone());
    let source = source
        .map(|source| plan_document::with_vault_arms(source, &rewrites))
        .transpose()
        .context("failed to make the plan document to store")?;
    let submitted_sha256 = submitted.filter(|submitted| {
        source
            .as_ref()
            .is_some_and(|stored| stored.sha256() != submitted)
    });

    let default_expiry = ExpiresAt::At(now + Duration::days(DEFAULT_EXPIRY_DAYS));

    // Place every input on a job: a fresh mint, or one already running the
    // cell. This step owns every content-key write and membership marker, so
    // the plan's claim on each shared job is durable once it returns.
    let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
    let context = MergeContext {
        plans,
        todo,
        submissions,
        plan_id: &plan_id,
        // Filled on the first index hit; a plan of all-new cells never lists
        // `leased/` at all.
        leased: tokio::sync::OnceCell::new(),
        now,
        creating_max_age,
    };
    let inputs: Vec<Input> = jobs
        .into_iter()
        .map(
            |GatedJob {
                 label,
                 body,
                 hf_model,
             }| {
                let horizon = resolve_expires_at(&body, default_expiry);
                Input {
                    label,
                    body,
                    horizon,
                    hf_model,
                }
            },
        )
        .collect();
    let grouped = Grouping::of(inputs.iter().map(|input| (&input.body, input.horizon)));
    let scope = cell_scope(&membership.owner, orgs).await?;
    let CampaignMembership {
        campaign_id,
        supersedes,
        carry,
        ..
    } = membership;
    // Drawn before the units overlap, so the draw is independent of their
    // order, and before the first placement, which is the first write.
    let Draw {
        mut inherited,
        rerun_of: carried_rerun_of,
        unclaimed,
    } = carry.draw(&grouped.units)?;
    unclaimed
        .answer(
            &grouped.units,
            |unit| inputs.get(unit.lead).map(|input| &input.body),
            &mut inherited,
            context.submissions,
        )
        .await?;
    let satisfied = satisfy_from_earlier_runs(
        &grouped.units,
        |unit| inputs.get(unit.lead).map(|input| &input.body),
        &mut inherited,
        context.plans,
        context.submissions,
        &scope,
    )
    .await?;
    satisfied.iter().for_each(|satisfaction| {
        let unit = grouped.units.get(satisfaction.unit);
        tracing::info!(
            plan_id = %plan_id,
            label = unit
                .and_then(|unit| inputs.get(unit.lead))
                .map(|input| input.label.as_str()),
            content_key = unit.map(|unit| unit.digest.as_str()),
            cell_key = %satisfaction.cell,
            scope = %scope,
            job_id = %satisfaction.job_id,
            carried_job_id = satisfaction.carried.as_ref().map(JobId::as_str),
            "cell satisfied by an earlier finished run"
        );
    });
    // `buffered`: results are read back by unit index. Indices, not `&Unit`: a
    // closure returning a future that borrows its argument trips higher-ranked
    // lifetime inference, with an error that names `router.rs`.
    let (units, leads, placing, inherited) = (&grouped.units, &inputs, &context, &inherited);
    let outcomes: Vec<Placed> = futures::stream::iter(0..units.len())
        .map(|index| async move {
            let unit = units.get(index).context("unit index out of range")?;
            let lead = leads.get(unit.lead).context("unit lead out of range")?;
            let inherited = inherited.get(index).context("unit draw out of range")?;
            placing.place(unit, &lead.body, inherited).await
        })
        .buffered(PLAN_JOB_CONCURRENCY)
        .try_collect()
        .await?;
    let mut placements: Vec<Placement> = inputs
        .into_iter()
        .zip(&grouped.unit_of)
        .enumerate()
        .map(|(index, (input, &input_unit))| {
            let (unit, outcome) = grouped
                .units
                .get(input_unit)
                .zip(outcomes.get(input_unit))
                .context("input's unit was never placed")?;
            let placed = unit.outcome_for(index, outcome);
            // Only the input that mints answers or re-runs the job; its
            // siblings share it.
            let parent = match (&placed, inherited.get(input_unit)) {
                (Placed::Fresh, Some(Inherited::Recalculate(source))) => {
                    Some(Parent::Recalculates(source.clone()))
                }
                (Placed::Fresh, Some(Inherited::Rerun(of))) => of.clone().map(Parent::Reruns),
                _ => None,
            };
            anyhow::Ok(Placement::new(input, unit, placed, parent))
        })
        .collect::<anyhow::Result<_>>()?;
    // A minted job carries the latest horizon of its inputs, the last moment
    // the plan wants an answer, stamped back into each body so key and body
    // agree. A job this plan joined keeps its key's deadline.
    let deadlines = minted_deadlines(&placements);
    placements.iter_mut().for_each(|p| {
        if let Some(deadline) = deadlines.get(&p.job_id) {
            p.retime(*deadline);
        }
    });

    let mut seen: HashSet<&JobId> = HashSet::new();
    let distinct: Vec<&Placement> = placements
        .iter()
        .filter(|p| seen.insert(&p.job_id))
        .collect();
    let job_ids: Vec<JobId> = distinct.iter().map(|p| p.job_id.clone()).collect();
    // Only a plan filed from a document can be revised, so only it pays for
    // the map.
    let content_keys: BTreeMap<JobId, String> = if source.is_some() {
        distinct
            .iter()
            .map(|p| (p.job_id.clone(), p.digest.clone()))
            .collect()
    } else {
        BTreeMap::new()
    };
    // A job this plan re-runs names its parent, and a job it carries keeps the
    // name the superseded plan gave it.
    let rerun_of: BTreeMap<JobId, JobId> = distinct
        .iter()
        .filter_map(|p| {
            let parent = match &p.parent {
                Some(Parent::Reruns(of)) => Some(of.clone()),
                _ => carried_rerun_of.get(&p.job_id).cloned(),
            }?;
            Some((p.job_id.clone(), parent))
        })
        .collect();
    let job_expiry = distinct
        .iter()
        .map(|p| p.expires_at)
        .min()
        .zip(distinct.iter().map(|p| p.expires_at).max())
        .map(|(earliest, latest)| JobExpiry { earliest, latest });

    // The source lands before the manifest that names it, and the pending
    // marker before the source, so an ingest that stops early leaves a marker
    // `queue-maintenance` finds.
    if let Some(source) = &source {
        plans
            .write_pending_source_marker(&plan_id)
            .await
            .with_context(|| format!("writing the pending source marker for {plan_id}"))?;
        let outcome = plans
            .put_plan_source(&plan_id, source)
            .await
            .with_context(|| format!("writing the plan source for {plan_id}"))?;
        // The plan id is fresh, so an existing source is a bug: stop before a
        // manifest can name the wrong document.
        if outcome == CreateOutcome::AlreadyExists {
            tracing::error!(
                plan_id = %plan_id,
                "plan source already exists for a newly minted plan id"
            );
            return Err(IngestError::Other(PlanSourceExistsError { plan_id }.into()));
        }
    }

    // The `creating` manifest is the authoritative job-id list, written before
    // anything is claimable.
    let creating = PlanManifest {
        plan_id: plan_id.clone(),
        status: PlanStatus::Creating,
        created_at: now,
        submitted_by: submitted_by.clone(),
        previous_plan_id: supersedes,
        source_sha256: source.as_ref().map(|source| source.sha256().clone()),
        submitted_sha256,
        plan_warnings: plan_warnings.clone(),
        benchmark_types,
        job_ids: job_ids.clone(),
        content_keys,
        rerun_of,
        campaign_id: Some(campaign_id),
        warnings: Vec::new(),
        // Only queue-maintenance records a lapse, so a fresh plan has none.
        expired_jobs: std::collections::BTreeMap::new(),
        job_expiry,
        settled_jobs: Default::default(),
        progress_snapshot: None,
        terminal_at: None,
    };
    plans
        .put_plan(&creating)
        .await
        .with_context(|| format!("writing creating manifest for {plan_id}"))?;
    let notes = PlanNotes {
        note: note.clone().map(Into::into),
        change: change.clone(),
    };
    if !notes.is_empty() {
        plans
            .put_plan_notes(&plan_id, &notes)
            .await
            .with_context(|| format!("writing the notes of {plan_id}"))?;
    }

    // A marker left behind costs the sweep one manifest read, so a failed
    // delete is not a failed ingest.
    if source.is_some()
        && let Err(e) = plans.delete_pending_source_marker(&plan_id).await
    {
        tracing::warn!(
            plan_id = %plan_id,
            error = %e,
            "failed to delete the pending marker of a stored plan source"
        );
    }

    // Per job: the source, then the link, then for a minted job its cell, the
    // marker and the promote into `avail/`, so no claimable job lacks its source or its
    // minting plan's marker. A recalculated job goes to `incoming/` instead.
    // The minter writes a marker too, so an absent marker means released or
    // lapsed, which `expired_jobs` tells apart. Jobs run concurrently.
    let staging: Vec<_> = distinct
        .iter()
        .map(|p| {
            let (plan_id, scope) = (&plan_id, &scope);
            async move {
                let stored = plans
                    .put_job_source(&p.job_id, &p.body)
                    .await
                    .with_context(|| {
                        format!("storing the source of job {} for {plan_id}", p.job_id)
                    })?;
                plans
                    .put_job_plan_link(&p.job_id, plan_id)
                    .await
                    .with_context(|| format!("linking job {} to {plan_id}", p.job_id))?;
                if p.merged.is_none() {
                    // Best effort: a job without its cell is never offered to a
                    // later plan, which then runs the cell again.
                    let cell_key = crate::content_key::cell_digest(&p.body);
                    if let Err(e) = plans.put_job_cell(&p.job_id, &cell_key, scope).await {
                        tracing::warn!(
                            plan_id = %plan_id,
                            job_id = %p.job_id,
                            cell_key = %cell_key,
                            scope = %scope,
                            error = %e,
                            "failed to index the cell of a minted job"
                        );
                    }
                }
                if let Some(Parent::Recalculates(source)) = &p.parent {
                    // Answered rather than queued, so it holds no marker, as a
                    // carried job with a result holds none.
                    recalculation::answer_from(submissions, source, &p.job_id, now)
                        .await
                        .with_context(|| {
                            format!("answering job {} from {source} for {plan_id}", p.job_id)
                        })?;
                } else if p.merged.is_none() {
                    plans
                        .write_job_plan_marker(&p.job_id, plan_id, p.expires_at)
                        .await
                        .with_context(|| {
                            format!("recording {plan_id} as a member of {}", p.job_id)
                        })?;
                    todo.write_tmp(&p.job_id, &p.body)
                        .await
                        .with_context(|| format!("staging job {} for {plan_id}", p.job_id))?;
                    todo.promote_avail(&p.job_id, p.expires_at)
                        .await
                        .with_context(|| format!("promoting job {} for {plan_id}", p.job_id))?;
                }
                anyhow::Ok(stored)
            }
        })
        .collect();
    let staged = futures::stream::iter(staging)
        .buffer_unordered(PLAN_JOB_CONCURRENCY)
        .try_collect::<Vec<CreateOutcome>>();
    // The roster is read beside the staging writes, which it does not depend on.
    let (stored, clients, permissions, tag_markers) = futures::try_join!(
        staged,
        async {
            drain_clients(auth)
                .await
                .context("listing clients for fleet match")
        },
        async {
            auth.list_client_permissions()
                .await
                .context("listing client permissions for fleet match")
        },
        async {
            tags.list_forward_tag_markers()
                .await
                .context("listing client tags for fleet match")
        },
    )?;
    let job_sources_created = stored
        .iter()
        .filter(|outcome| **outcome == CreateOutcome::Created)
        .count();
    let job_sources_existing = stored.len() - job_sources_created;

    // One fleet-match pass drives both the warnings and the status. The content
    // key spans every eligibility clause, so a joined job's body matches this
    // one's. The roster is what the claim path admits: approved and holding
    // `perform_work`.
    warn_unreadable_clients(&clients.unreadable);
    let rights = matching::FleetRights::new(permissions);
    let workers = rights.workers(&clients.items);
    let fleet_tags = matching::FleetTags::new(tag_markers);
    // A job with a result waits on no client.
    let outstanding: Vec<&Placement> = distinct.iter().copied().filter(|p| !p.settled).collect();
    let unmatched: Vec<&Placement> = outstanding
        .iter()
        .copied()
        .filter(|p| {
            !workers
                .iter()
                .any(|c| job_matches_client(&p.body, c, &fleet_tags))
        })
        .collect();
    // Nothing outstanding reads `active` until `queue-maintenance` latches it.
    let status = if !outstanding.is_empty() && unmatched.len() == outstanding.len() {
        PlanStatus::PendingClients
    } else {
        PlanStatus::Active
    };
    // The fleet warnings come first: they decide the plan's status.
    let mut warnings = group_warnings(&unmatched);
    warnings.extend(uncatalogued_model_warnings(models, &distinct));

    let final_manifest = PlanManifest {
        status,
        warnings: warnings.clone(),
        ..creating
    };
    plans
        .put_plan(&final_manifest)
        .await
        .with_context(|| format!("finalizing manifest for {plan_id}"))?;

    let merges: Vec<MergedInput> = placements
        .iter()
        .filter_map(|p| {
            p.merged.map(|kind| MergedInput {
                label: p.label.clone(),
                job_id: p.job_id.clone(),
                kind,
            })
        })
        .collect();
    tracing::info!(
        plan_id = %plan_id,
        job_count = job_ids.len(),
        merged_job_count = merges.len(),
        satisfied_job_count = merges
            .iter()
            .filter(|merge| merge.kind == MergeKind::Satisfied)
            .count(),
        status = %status.label(),
        // Absent for a plan the CLI filed. Logged here, where the durable record
        // gains the identity.
        submitted_by = submitted_by.as_ref().map(UserId::as_str),
        // Absent for a plan filed as job bodies.
        source_sha256 = source.as_ref().map(|source| source.sha256().as_ref()),
        plan_warnings = plan_warnings.len(),
        job_sources_created,
        job_sources_existing,
        "plan created",
    );
    if !warnings.is_empty() {
        tracing::info!(
            plan_id = %plan_id,
            warning_groups = warnings.len(),
            "plan ingested with warnings",
        );
    }

    Ok(IngestReport {
        plan_id,
        note,
        change,
        job_count: job_ids.len(),
        merged_job_count: merges.len(),
        job_ids: placements
            .into_iter()
            .map(|p| (p.label, p.job_id))
            .collect(),
        merges,
        warnings,
        plan_warnings,
    })
}

/// What a revision's cells would take from the plan it supersedes, for a dry
/// run: [`ingest_jobs`]'s draw without the writes, refused as it refuses a
/// re-run whose document no longer places every cell of the plan it re-runs.
pub async fn draw_carry(
    membership: CampaignMembership,
    bodies: &[Value],
    plans: &dyn PlanStore,
    submissions: &dyn SubmissionStore,
    orgs: &dyn OrgStore,
) -> Result<CarryDraw, IngestError> {
    let scope = cell_scope(&membership.owner, orgs).await?;
    let grouped = Grouping::of(bodies.iter().map(|body| (body, ExpiresAt::Never)));
    let head = membership.supersedes.clone();
    let Draw {
        mut inherited,
        unclaimed,
        ..
    } = membership.carry.draw(&grouped.units)?;
    let Answers {
        requirements_rerun,
        offered,
    } = unclaimed
        .answer(
            &grouped.units,
            |unit| bodies.get(unit.lead),
            &mut inherited,
            submissions,
        )
        .await?;
    let satisfied = satisfy_from_earlier_runs(
        &grouped.units,
        |unit| bodies.get(unit.lead),
        &mut inherited,
        plans,
        submissions,
        &scope,
    )
    .await?
    .len();
    // An earlier run can answer a cell that the head ran under other
    // requirements, so such a cell runs again only if it still has nothing.
    let requirements_rerun = requirements_rerun
        .into_iter()
        .filter(|&unit| matches!(inherited.get(unit), Some(Inherited::Nothing)))
        .count();
    // The plan each unit's job comes from, `None` for a job it does not hold.
    let held: Vec<Option<(PlanId, JobId)>> = inherited
        .iter()
        .map(|inherited| match inherited {
            Inherited::Job(job_id) | Inherited::Answered(job_id) => offered
                .get(job_id)
                .or(head.as_ref())
                .map(|plan_id| (plan_id.clone(), job_id.clone())),
            Inherited::Nothing
            | Inherited::Rerun(_)
            | Inherited::Recalculate(_)
            | Inherited::Satisfied(_) => None,
        })
        .collect();
    let drawn = inherited
        .into_iter()
        .fold(CarryDraw::default(), |mut drawn, inherited| {
            match inherited {
                Inherited::Answered(job_id) if offered.contains_key(&job_id) => {}
                Inherited::Job(job_id) | Inherited::Answered(job_id) => drawn.carried.push(job_id),
                Inherited::Nothing => drawn.new_cells += 1,
                Inherited::Rerun(_) | Inherited::Recalculate(_) | Inherited::Satisfied(_) => {}
            }
            drawn
        });
    let held = grouped
        .unit_of
        .iter()
        .enumerate()
        .filter_map(|(body_index, &unit)| {
            let (plan_id, job_id) = held.get(unit)?.clone()?;
            Some(HeldJob {
                body_index,
                plan_id,
                job_id,
            })
        })
        .collect();
    // Such a cell drew nothing, and the head did run it.
    Ok(CarryDraw {
        new_cells: drawn.new_cells.saturating_sub(requirements_rerun),
        requirements_rerun,
        satisfied,
        offered,
        held,
        ..drawn
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CarryDraw {
    /// The superseded plan's jobs the revision would hold.
    pub carried: Vec<JobId>,
    /// Cells the superseded plan never ran, nor a re-run or a recalculation
    /// names.
    pub new_cells: usize,
    /// Cells the superseded plan ran that the revision runs again: the run
    /// failed, or its device or build is not known to meet the cell's clauses.
    pub requirements_rerun: usize,
    /// Cells that an earlier finished run answers
    /// ([`satisfy_from_earlier_runs`]). The revision holds that run and queues
    /// nothing for the cell.
    pub satisfied: usize,
    /// The other plans' jobs the revision would hold, with the plan each comes
    /// from ([`Carryover::with_offered`]).
    pub offered: HashMap<JobId, PlanId>,
    /// Each body that would hold an existing job, and the plan it comes from.
    pub held: Vec<HeldJob>,
}

/// One submitted job body, with the horizon its `expires_at` resolves to.
struct Input {
    label: String,
    body: Value,
    horizon: ExpiresAt,
    /// The HuggingFace model that the import replaced, without its token,
    /// as [`GatedJob::hf_model`] holds it.
    hf_model: Option<Value>,
}

/// One placement the merge pass runs, for every input of this ingest on one
/// cell: a second pass over the digest would only race the first for its own
/// content key. The lead is the input that mints if the cell is new.
struct Unit {
    /// Index of the first input on this cell, in submission order.
    lead: usize,
    /// The latest horizon among the inputs on this unit: a plan's effective
    /// horizon is the latest of its markers, so one marker suffices.
    horizon: ExpiresAt,
    /// The id the lead mints under, minted before placement so it is fixed
    /// whatever order the units complete in.
    candidate: JobId,
    /// The cell every input on this unit describes.
    digest: String,
}

impl Unit {
    /// A unit that joined hands every member the same target; one that minted
    /// hands each sibling a within-plan merge onto the lead's job.
    fn outcome_for(&self, index: usize, placed: &Placed) -> Placed {
        match placed {
            Placed::Join(target) => Placed::Join(target.clone()),
            Placed::Fresh if index == self.lead => Placed::Fresh,
            Placed::Fresh => Placed::Join(JoinTarget {
                job_id: self.candidate.clone(),
                // Need only agree with `minted_deadlines`, which re-stamps
                // every body.
                expires_at: self.horizon,
                kind: MergeKind::WithinPlan,
                settled: false,
            }),
        }
    }
}

/// The inputs of one ingest, grouped onto the placements that serve them.
struct Grouping {
    units: Vec<Unit>,
    /// The unit each input belongs to, indexed by input.
    unit_of: Vec<usize>,
}

impl Grouping {
    /// Group inputs by content key, decidable from the bodies alone. An input
    /// that opted out is always its own unit: a plan that measures variance
    /// wants every duplicate to run.
    fn of<'a>(inputs: impl IntoIterator<Item = (&'a Value, ExpiresAt)>) -> Self {
        let mut units: Vec<Unit> = Vec::new();
        let mut by_digest: HashMap<String, usize> = HashMap::new();
        let unit_of: Vec<usize> = inputs
            .into_iter()
            .enumerate()
            .map(|(index, (body, horizon))| {
                let digest = content_digest(body);
                let is_mergeable = mergeable(body);
                let existing = is_mergeable
                    .then(|| by_digest.get(&digest).copied())
                    .flatten()
                    .and_then(|unit| Some((unit, units.get_mut(unit)?)));
                match existing {
                    Some((unit, existing)) => {
                        existing.horizon = existing.horizon.max(horizon);
                        unit
                    }
                    None => {
                        let unit = units.len();
                        if is_mergeable {
                            by_digest.insert(digest.clone(), unit);
                        }
                        units.push(Unit {
                            lead: index,
                            horizon,
                            candidate: JobId::from_uuid(uuid::Uuid::now_v7()),
                            digest,
                        });
                        unit
                    }
                }
            })
            .collect();
        Self { units, unit_of }
    }
}

/// One input on its way from placement to the report.
struct Placement {
    label: String,
    job_id: JobId,
    /// The input body with `job_id` and the deadline stamped in: the job source,
    /// the `avail/` body of a minted job, and what the fleet match reads.
    body: Value,
    /// The deadline the job carries in the queue: for a job this plan mints, the
    /// latest horizon of every input placed on it; for a merged one, the
    /// incumbent's.
    expires_at: ExpiresAt,
    /// The horizon this input alone asked for. It is the deadline this plan
    /// holds the job under, and for a joined job it may sit either side of the
    /// job's own deadline.
    horizon: ExpiresAt,
    /// `None` when this input minted the job.
    merged: Option<MergeKind>,
    /// The unit's digest, recorded in [`PlanManifest::content_keys`].
    digest: String,
    /// Whether the job already has a result ([`JoinTarget::settled`]), or
    /// staging answers it.
    settled: bool,
    /// The superseded plan's job the job this input mints comes from, for a
    /// recalculation or a re-run.
    parent: Option<Parent>,
    /// The HuggingFace model that the import replaced, without its token,
    /// as [`GatedJob::hf_model`] holds it.
    hf_model: Option<Value>,
}

/// The superseded plan's job a minted job comes from.
#[derive(Debug, Clone)]
enum Parent {
    /// Its record answers the minted job.
    Recalculates(JobId),
    /// The minted job runs it again.
    Reruns(JobId),
}

impl Placement {
    fn new(input: Input, unit: &Unit, placed: Placed, parent: Option<Parent>) -> Self {
        let Input {
            label,
            mut body,
            horizon,
            hf_model,
        } = input;
        let (job_id, expires_at, merged, settled) = match placed {
            Placed::Fresh => (unit.candidate.clone(), horizon, None, false),
            Placed::Join(target) => (
                target.job_id,
                target.expires_at,
                Some(target.kind),
                target.settled,
            ),
        };
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "job_id".to_string(),
                Value::String(job_id.as_str().to_string()),
            );
        }
        let mut placement = Placement {
            label,
            job_id,
            body,
            expires_at,
            horizon,
            merged,
            digest: unit.digest.clone(),
            settled: settled || matches!(parent, Some(Parent::Recalculates(_))),
            parent,
            hf_model,
        };
        placement.retime(expires_at);
        placement
    }

    /// Move this placement onto its job's deadline, and stamp that back into the
    /// body in canonical form. Scheduling reads the `avail/` filename; the body's
    /// copy lets the claim path warn when the two disagree.
    fn retime(&mut self, expires_at: ExpiresAt) {
        self.expires_at = expires_at;
        if let Some(obj) = self.body.as_object_mut() {
            obj.insert(
                "expires_at".to_string(),
                Value::String(expires_at.to_string()),
            );
        }
    }
}

/// The deadline each job this ingest mints carries: the latest horizon across
/// its inputs. A joined job is absent: it keeps its key's deadline, and the
/// plan's horizon goes in its membership marker.
fn minted_deadlines(placements: &[Placement]) -> HashMap<JobId, ExpiresAt> {
    let minted: HashSet<&JobId> = placements
        .iter()
        .filter(|p| p.merged.is_none())
        .map(|p| &p.job_id)
        .collect();
    placements
        .iter()
        .filter(|p| minted.contains(&p.job_id))
        .fold(HashMap::new(), |mut acc, p| {
            acc.entry(p.job_id.clone())
                .and_modify(|latest| *latest = (*latest).max(p.horizon))
                .or_insert(p.horizon);
            acc
        })
}

/// A job an input shares rather than mints: what a join reports, and what
/// [`Unit::outcome_for`] hands the siblings of an input that minted.
#[derive(Clone)]
struct JoinTarget {
    job_id: JobId,
    /// The job's official deadline, the value its `avail/` key carries. A joiner
    /// takes it as given; its own horizon lives in its membership marker.
    expires_at: ExpiresAt,
    /// What an input reports when it joins this job. A sibling of a job this
    /// ingest minted took the cell from this same plan; a sibling of a job this
    /// ingest joined took it from the plan that owns that job.
    kind: MergeKind,
    /// The job left the queue with a result, so the fleet match skips it. A
    /// carried job and a satisfied one are placed this way.
    settled: bool,
}

/// Where one input's job comes from.
enum Placed {
    Fresh,
    /// Share a job that already exists, under the deadline that job carries.
    Join(JoinTarget),
}

/// What the merge pass reads: the stores, the ingest's own identity, and the two
/// deadlines its rules compare against.
struct MergeContext<'a> {
    plans: &'a dyn PlanStore,
    todo: &'a dyn TodoStore,
    submissions: &'a dyn SubmissionStore,
    plan_id: &'a PlanId,
    /// Every `leased/` job's deadline, listed at most once per ingest. A fast
    /// path only: `official_state` confirms against a fresh listing before it
    /// reads a job as gone.
    leased: tokio::sync::OnceCell<HashMap<JobId, ExpiresAt>>,
    now: DateTime<Utc>,
    /// `plan_creating_max_age_secs`. One gate decides whether a content-key
    /// record whose job is not live belongs to an ingest that is still staging
    /// (leave it alone) or to one that provably died (take the digest over).
    creating_max_age: std::time::Duration,
}

/// How an attempt to join the job a content-key record names came out.
enum JoinAttempt {
    /// The job is live and unlapsed, and the membership marker is written and
    /// confirmed.
    Joined(JoinTarget),
    /// Not a job this input joins, and the record stands: the target is queued
    /// under an unreadable deadline, or absent or lapsed with a record young
    /// enough to be still staging. The caller mints either way.
    Declined,
    /// The record names a job that is provably gone, so the digest is available.
    Stale,
}

/// What the queue says about a merge target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetState {
    /// Queued, under the official deadline it carries.
    Live(ExpiresAt),
    /// Queued, and its deadline cannot be read.
    LiveUnreadable,
    /// In neither `avail/` nor `leased/`.
    Absent,
}

impl MergeContext<'_> {
    /// Place one unit: mint a fresh job, or join one that already runs the cell.
    /// `body` must be the lead's, since its digest is published with it.
    /// Units overlap safely: mergeable units never share a digest, opted-out
    /// ones read no index, and the `leased/` snapshot is read-only once filled.
    async fn place(
        &self,
        unit: &Unit,
        body: &Value,
        inherited: &Inherited,
    ) -> anyhow::Result<Placed> {
        let Unit {
            digest,
            candidate,
            horizon,
            ..
        } = unit;

        // The superseded plan's job, and an earlier run that answers the cell,
        // come ahead of the index and the opt-out. A re-run mints: the index may
        // still name the answered job it re-runs.
        match inherited {
            Inherited::Job(job_id) => {
                if let Some(placed) = self.carry(digest, job_id, *horizon).await? {
                    return Ok(placed);
                }
            }
            Inherited::Rerun(_) => return self.claim_unmerged(digest, candidate, "rerun").await,
            // Answered at staging and never queued, so no later plan can join
            // it, and the index is left alone.
            Inherited::Recalculate(_) => return Ok(Placed::Fresh),
            // Its record was read at the draw, and a recorded job is finished
            // whatever the queue still holds of it.
            Inherited::Answered(job_id) => return Ok(self.carried(digest, job_id, *horizon, true)),
            // Finished, so it waits on no client, and no marker holds it.
            Inherited::Satisfied(job_id) => {
                return Ok(Placed::Join(JoinTarget {
                    job_id: job_id.clone(),
                    expires_at: *horizon,
                    kind: MergeKind::Satisfied,
                    settled: true,
                }));
            }
            Inherited::Nothing => {}
        }

        // Opted out: read no index, not even to join a sibling. The key is still
        // published, so a later plan can join the job this mints.
        if !mergeable(body) {
            return self.claim_unmerged(digest, candidate, "opted_out").await;
        }

        match self.plans.get_content_key(digest).await? {
            None => self.claim(digest, candidate, *horizon).await,
            Some((record, version)) => {
                self.resolve(digest, &record, &version, candidate, *horizon)
                    .await
            }
        }
    }

    /// Take over the superseded plan's `job_id`, or `None` to place the unit as
    /// any other. A live job is joined as [`Self::try_join`] joins one, so the
    /// old plan's teardown releases it. The result is read first: a finished job
    /// is the common case, and a queue miss costs a fresh `leased/` listing.
    async fn carry(
        &self,
        digest: &str,
        job_id: &JobId,
        horizon: ExpiresAt,
    ) -> anyhow::Result<Option<Placed>> {
        let carried = |expires_at, settled| Some(self.carried(digest, job_id, expires_at, settled));
        if self.submissions.find_job(job_id).await?.is_some() {
            return Ok(carried(horizon, true));
        }
        match self.official_state(job_id).await? {
            TargetState::Live(official) if !official.is_expired(Utc::now()) => {
                self.plans
                    .write_job_plan_marker(job_id, self.plan_id, horizon)
                    .await?;
                if self.target_survives(job_id).await? {
                    return Ok(carried(official, false));
                }
                if let Err(e) = self
                    .plans
                    .delete_job_plan_marker(job_id, self.plan_id, horizon)
                    .await
                {
                    tracing::warn!(
                        plan_id = %self.plan_id,
                        job_id = %job_id,
                        error = %e,
                        "failed to drop the membership marker of a vanished carried job"
                    );
                }
                Ok(None)
            }
            TargetState::Live(_) | TargetState::LiveUnreadable => Ok(None),
            // It may have finished since the first read: a submission records
            // before it leaves the queue.
            TargetState::Absent => Ok(match self.submissions.find_job(job_id).await? {
                Some(_) => carried(horizon, true),
                None => None,
            }),
        }
    }

    /// The superseded plan's `job_id` held under `expires_at`, `settled` when
    /// it has a record.
    fn carried(
        &self,
        digest: &str,
        job_id: &JobId,
        expires_at: ExpiresAt,
        settled: bool,
    ) -> Placed {
        tracing::info!(
            plan_id = %self.plan_id,
            job_id = %job_id,
            content_key = %digest,
            settled,
            kind = MergeKind::Carried.label(),
            "job carried over from the superseded plan"
        );
        Placed::Join(JoinTarget {
            job_id: job_id.clone(),
            expires_at,
            kind: MergeKind::Carried,
            settled,
        })
    }

    /// Publish the content key of a job minted without reading the index, so a
    /// later plan can join it. Create-if-absent: an incumbent keeps the digest,
    /// even a provably gone one, which the merge path and the index GC handle.
    async fn claim_unmerged(
        &self,
        digest: &str,
        candidate: &JobId,
        reason: &'static str,
    ) -> anyhow::Result<Placed> {
        match self
            .plans
            .create_content_key(digest, &self.record_for(candidate))
            .await?
        {
            ContentKeyClaim::Created => tracing::info!(
                plan_id = %self.plan_id,
                job_id = %candidate,
                content_key = %digest,
                reason,
                "minted without merging; published the content key of the job it minted"
            ),
            ContentKeyClaim::AlreadyExists(record, _) => tracing::info!(
                plan_id = %self.plan_id,
                job_id = %candidate,
                content_key = %digest,
                incumbent_job_id = %record.job_id,
                reason,
                "minted without merging; the content key already names a job, so it stands"
            ),
        }
        Ok(Placed::Fresh)
    }

    /// Dispatch one [`Self::try_join`] outcome: take the join, mint alongside a
    /// record that stands, or take a provably-orphaned digest over.
    async fn resolve(
        &self,
        digest: &str,
        record: &ContentKeyRecord,
        version: &ContentKeyVersion,
        candidate: &JobId,
        plan_expiry: ExpiresAt,
    ) -> anyhow::Result<Placed> {
        match self.try_join(digest, record, plan_expiry).await? {
            JoinAttempt::Joined(target) => Ok(Placed::Join(target)),
            JoinAttempt::Declined => Ok(Placed::Fresh),
            JoinAttempt::Stale => {
                self.replace_stale(digest, version, candidate, plan_expiry)
                    .await
            }
        }
    }

    /// Claim an unclaimed digest for a fresh job. The claim lands **before** the
    /// job promotes, so a second ingest of the same cell either reads this record
    /// and joins, or loses the create and joins the winner.
    async fn claim(
        &self,
        digest: &str,
        candidate: &JobId,
        plan_expiry: ExpiresAt,
    ) -> anyhow::Result<Placed> {
        match self
            .plans
            .create_content_key(digest, &self.record_for(candidate))
            .await?
        {
            ContentKeyClaim::Created => Ok(Placed::Fresh),
            // A concurrent ingest of the same cell got there first. Its record is
            // in hand, so evaluate the join against it rather than re-reading.
            ContentKeyClaim::AlreadyExists(record, version) => {
                self.resolve(digest, &record, &version, candidate, plan_expiry)
                    .await
            }
        }
    }

    /// Try to join the job a content-key record names, leaving the record
    /// untouched. The job's deadline must still be in the future: a lapsed
    /// entry is dead, and a joiner would inherit its synthetic failure. This
    /// plan's horizon may fall either side of that deadline: the marker records
    /// it, and the expiry pass re-dates.
    ///
    /// The marker lands before a fresh liveness re-check in state-flow order
    /// (`avail/`, `leased/`, `avail/` again, the submission record). Forward
    /// transitions write their destination first; the second `avail/` read
    /// covers a recycle or restore moving the job back. A miss everywhere means
    /// teardown retired it, so the marker is dropped and the caller mints.
    async fn try_join(
        &self,
        digest: &str,
        record: &ContentKeyRecord,
        plan_expiry: ExpiresAt,
    ) -> anyhow::Result<JoinAttempt> {
        let target = &record.job_id;
        let official = match self.official_state(target).await? {
            TargetState::Live(official) => official,
            // Minting is the only safe direction: reading it as absent would let
            // [`Self::replace_stale`] take the digest over from a live job.
            TargetState::LiveUnreadable => {
                tracing::warn!(
                    plan_id = %self.plan_id,
                    job_id = %target,
                    content_key = %digest,
                    "declined to merge: the target is queued and its deadline is unreadable"
                );
                return Ok(JoinAttempt::Declined);
            }
            TargetState::Absent => return Ok(self.classify_absent(record)),
        };
        // A fresh clock: a long ingest can outlive the target's deadline, and the
        // stale clock would still join a lapsed entry.
        if official.is_expired(Utc::now()) {
            // The entry is past its deadline in place. The sweep will resolve it
            // to a synthetic failure, so it is dead already.
            return Ok(self.classify_absent(record));
        }
        self.plans
            .write_job_plan_marker(target, self.plan_id, plan_expiry)
            .await?;
        if !self.target_survives(target).await? {
            // Retired between the lookup and the marker. Dropping the marker is
            // best effort: the age-gated GC collects one left behind.
            if let Err(e) = self
                .plans
                .delete_job_plan_marker(target, self.plan_id, plan_expiry)
                .await
            {
                tracing::warn!(
                    plan_id = %self.plan_id,
                    job_id = %target,
                    error = %e,
                    "failed to drop the membership marker of a vanished merge target"
                );
            }
            tracing::info!(
                plan_id = %self.plan_id,
                job_id = %target,
                content_key = %digest,
                "merge target vanished after the membership write; minting a fresh job"
            );
            return Ok(JoinAttempt::Stale);
        }

        let joined = JoinTarget {
            job_id: target.clone(),
            expires_at: official,
            // A sibling that joins this entry takes the cell from the plan that
            // owns the job, not from this one.
            kind: MergeKind::AcrossPlans,
            settled: false,
        };
        tracing::info!(
            plan_id = %self.plan_id,
            job_id = %target,
            content_key = %digest,
            owning_plan_id = %record.plan_id,
            kind = MergeKind::AcrossPlans.label(),
            "job merged onto an existing job"
        );
        Ok(JoinAttempt::Joined(joined))
    }

    /// Whether a record whose job is not live is unborn or dead: inside the age
    /// gate its ingest may still be staging, past it the stranded-`creating`
    /// teardown already ran.
    fn classify_absent(&self, record: &ContentKeyRecord) -> JoinAttempt {
        if past_gate(record.created_at, self.now, self.creating_max_age) {
            JoinAttempt::Stale
        } else {
            tracing::info!(
                plan_id = %self.plan_id,
                job_id = %record.job_id,
                owning_plan_id = %record.plan_id,
                "content key names a job that is not queued yet; minting a fresh job \
                 and leaving the record alone"
            );
            JoinAttempt::Declined
        }
    }

    /// Take a provably orphaned digest over, conditioned on the version read: a
    /// rival that claimed it since owns a live job. A lost swap re-reads and
    /// retries once, then mints unmerged: duplicate work, never a wrong merge.
    async fn replace_stale(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
        candidate: &JobId,
        plan_expiry: ExpiresAt,
    ) -> anyhow::Result<Placed> {
        match self
            .plans
            .replace_content_key(digest, version, &self.record_for(candidate))
            .await?
        {
            ContentKeyCas::Applied => {
                tracing::info!(
                    plan_id = %self.plan_id,
                    job_id = %candidate,
                    content_key = %digest,
                    "replaced the content key of a job that is gone"
                );
                Ok(Placed::Fresh)
            }
            ContentKeyCas::Lost => {
                if let Some((record, _)) = self.plans.get_content_key(digest).await?
                    && let JoinAttempt::Joined(target) =
                        self.try_join(digest, &record, plan_expiry).await?
                {
                    return Ok(Placed::Join(target));
                }
                tracing::warn!(
                    plan_id = %self.plan_id,
                    content_key = %digest,
                    "lost the content-key race twice; minting an unmerged job"
                );
                Ok(Placed::Fresh)
            }
        }
    }

    /// What the queue says about a merge target, by the deadline in its `avail/`
    /// or lease key. Reads `avail/`, the `leased/` snapshot, a fresh `leased/`
    /// listing, then `avail/` again for the backward moves [`Self::try_join`]
    /// names. The snapshot can only overstate liveness, which
    /// [`Self::target_survives`] re-checks.
    async fn official_state(&self, job_id: &JobId) -> anyhow::Result<TargetState> {
        if let Some(state) = self.avail_state(job_id).await? {
            return Ok(state);
        }
        let snapshot = self
            .leased
            .get_or_try_init(|| leased_expiries(self.todo))
            .await?;
        if let Some(expiry) = snapshot.get(job_id).copied() {
            return Ok(TargetState::Live(expiry));
        }
        if let Some(expiry) = leased_expiries(self.todo).await?.get(job_id).copied() {
            return Ok(TargetState::Live(expiry));
        }
        Ok(self
            .avail_state(job_id)
            .await?
            .unwrap_or(TargetState::Absent))
    }

    /// What an `avail/` entry says about the target; `None` is not absence, as
    /// `leased/` is next. The deadline comes from the key, which scheduling reads:
    /// the body's copy is never re-dated, so it would read a live job as lapsed
    /// and let [`Self::replace_stale`] take its key. An unreadable name is
    /// [`TargetState::LiveUnreadable`], never absence.
    async fn avail_state(&self, job_id: &JobId) -> anyhow::Result<Option<TargetState>> {
        let Some(name) = self.todo.avail_key_for_job(job_id).await? else {
            return Ok(None);
        };
        Ok(Some(match parse_avail_filename(&name) {
            Ok((_, expires_at)) => TargetState::Live(expires_at),
            Err(e) => {
                tracing::warn!(
                    plan_id = %self.plan_id,
                    job_id = %job_id,
                    key = %name,
                    error = %e,
                    "an avail/ key of a merge target does not parse; reading the job as queued"
                );
                TargetState::LiveUnreadable
            }
        }))
    }

    /// Whether the merge target still exists, read fresh in [`Self::try_join`]'s
    /// order. Never the `leased/` snapshot: it predates this plan's marker, and
    /// would read a retired job as live.
    async fn target_survives(&self, job_id: &JobId) -> anyhow::Result<bool> {
        if self.avail_state(job_id).await?.is_some() {
            return Ok(true);
        }
        if leased_expiries(self.todo).await?.contains_key(job_id) {
            return Ok(true);
        }
        // The recycler may have moved the job back into `avail/` while the two
        // reads above ran.
        if self.avail_state(job_id).await?.is_some() {
            return Ok(true);
        }
        // Completed inside the window: the plan reads the same submission record,
        // so the join stands.
        Ok(self.submissions.find_job(job_id).await?.is_some())
    }

    fn record_for(&self, job_id: &JobId) -> ContentKeyRecord {
        ContentKeyRecord {
            job_id: job_id.clone(),
            plan_id: self.plan_id.clone(),
            created_at: self.now,
        }
    }
}

/// The official deadline of every job in `leased/`, from one listing.
/// Unparseable keys are foreign cruft, already warned about by the maintenance
/// pass that owns the tree.
async fn leased_expiries(todo: &dyn TodoStore) -> anyhow::Result<HashMap<JobId, ExpiresAt>> {
    Ok(todo
        .list_leased()
        .await?
        .iter()
        .filter_map(|key| parse_leased_key(key).ok())
        .map(|lease| (lease.job_id, lease.expires_at))
        .collect())
}

/// Validation of a single job body; the caller names the job. The table
/// check and the recipe check read `hf_model` in place of `spec.model` when
/// given ([`model_aliases`]).
fn validate_job(
    body: &Value,
    hf_model: Option<&Value>,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    models: ModelCheck<'_>,
    now: DateTime<Utc>,
) -> Result<(), JobRejectionError> {
    let obj = body.as_object().ok_or(JobRejectionError::NotAnObject)?;

    // Identity is server-assigned; a pre-set id is a malformed handoff.
    if obj.contains_key("job_id") {
        return Err(JobRejectionError::PresetJobId);
    }
    if obj.contains_key("plan_id") {
        return Err(JobRejectionError::PresetPlanId);
    }

    // Shape before eligibility: the eligibility check reads a malformed clause
    // as absent, and would name the wrong fault. The classifier covers
    // `exclude_clients` too, so only `clients` needs the check below.
    if capability_clauses_malformed(body) {
        return Err(JobRejectionError::MalformedCapabilityClauses);
    }
    if let Some(clients) = obj.get("clients") {
        let arr = clients
            .as_array()
            .ok_or(JobRejectionError::ClientsNotAnArray)?;
        if arr.iter().any(|v| !v.is_string()) {
            return Err(JobRejectionError::ClientsNotStrings);
        }
    }

    let has_requires = non_empty_array(obj.get("requires"));
    let has_clients = non_empty_array(obj.get("clients"));
    if !has_requires && !has_clients {
        return Err(JobRejectionError::NoEligibility);
    }

    // Every capability flag must already be canonical (lowercase, no
    // whitespace); the only rewrite is the `:required` lift, done before this.
    if let Some(flag) = capability_flags(body).find(|f| *f != client::slugify(f)) {
        return Err(JobRejectionError::NonCanonicalFlag {
            flag: bounded(flag.to_string()),
        });
    }

    // At most one flag per reserved namespace in `requires`. `any_of` and
    // `excludes` are disjunctions, as `[chip:a15, chip:a16]` is, so exempt.
    let mut seen = BTreeSet::new();
    if let Some(namespace) = flat_flags(body, "requires")
        .filter_map(reserved_namespace)
        .find(|ns| !seen.insert(*ns))
    {
        return Err(JobRejectionError::DuplicateReservedNamespace {
            namespace: namespace.to_string(),
        });
    }

    // Reject a vacuous exclusion group: a typo there reads as a narrowing while
    // it silently widens eligibility.
    if let Some(namespace) = vacuous_exclusion_namespace(body) {
        return Err(JobRejectionError::VacuousExclusionGroup {
            namespace: namespace.to_string(),
        });
    }

    // Last of the eligibility checks, so a fault with a name of its own is
    // reported under that name rather than as a contradiction.
    if let Some(rule) = matching::unmatchable_reason(body) {
        return Err(JobRejectionError::StaticallyUnmatchable { rule });
    }

    // A string `"false"` would read as mergeable, the opposite of what the
    // author asked for.
    if let Err(other) = merge_duplicates(body) {
        return Err(JobRejectionError::MalformedMergeDuplicates {
            value: bounded(other.to_string()),
        });
    }

    match obj.get("expires_at") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if matches!(parse_expires_at(s), Some(ExpiresAt::At(at)) if at <= now) =>
        {
            return Err(JobRejectionError::PastExpiresAt {
                value: bounded(s.clone()),
            });
        }
        Some(Value::String(s)) if parse_expires_at(s).is_some() => {}
        // One rejection for any malformed value; its JSON rendering tells them
        // apart in the message.
        Some(other) => {
            return Err(JobRejectionError::MalformedExpiresAt {
                value: bounded(other.to_string()),
            });
        }
    }

    // Opaque but for `benchmark`, so the plan schema evolves in
    // `pipette-clients` without a release here. Requiring it refuses a
    // pre-envelope body that no client could run.
    let spec_value = obj.get("spec").ok_or(JobRejectionError::MissingSpec)?;
    let spec = spec_value
        .as_object()
        .ok_or(JobRejectionError::SpecNotAnObject)?;

    // The one spec field the server reads: it attributes synthetic failures
    // and fills the claim envelope's `benchmark_id`.
    let benchmark_id = spec
        .get("benchmark")
        .and_then(Value::as_str)
        .ok_or(JobRejectionError::MissingBenchmark)?;
    let benchmark_id =
        BenchmarkId::try_new(benchmark_id).map_err(JobRejectionError::InvalidBenchmarkId)?;
    if !catalog.contains_key(&benchmark_id) {
        let nearest = nearest_benchmark(catalog, &benchmark_id);
        return Err(JobRejectionError::UnknownBenchmark {
            id: benchmark_id,
            nearest,
        });
    }

    // Presence only: a truncated spec is refused here rather than leased out
    // to a certain client-side rejection.
    ["model", "runtime"]
        .into_iter()
        .try_for_each(|field| match spec.get(field) {
            Some(v) if !v.is_null() => Ok(()),
            _ => Err(JobRejectionError::MissingSpecField { field }),
        })?;

    // A spec this build parses must pair a runtime with a model it loads: a
    // client refuses one that does not, for good.
    if let Ok(run_spec) = ClientRunSpec::deserialize(spec_value) {
        run_spec
            .validate()
            .map_err(JobRejectionError::UnrunnableSpec)?;
        // Every device would mis-measure or fail such a cell, so refuse it here,
        // where the message can still name the knob.
        if let Some(eval) = catalog
            .get(&benchmark_id)
            .and_then(|benchmark| benchmark.def.as_eval().ok())
        {
            eval.check_cell_context(
                RuntimeType::of(&run_spec.runtime),
                run_spec.runtime_flags.as_ref(),
            )
            .map_err(|reason| JobRejectionError::EvalContext {
                benchmark: benchmark_id.clone(),
                reason,
            })?;
        }
    }

    // A score must be a vendor's recipe: the model it covers, in its thinking
    // mode, under the one entry that covers it for the task. Read off the two
    // fields the recipe judges, so a spec this build otherwise cannot parse (a
    // newer runtime, say) is held to it too; an imported model by its origin.
    // A vlm_eval's and an asr_eval's alike.
    if let Some(entry) = catalog
        .get(&benchmark_id)
        .and_then(|benchmark| RecipeEntry::of(&benchmark.def))
    {
        let recipe = entry.recipe;
        let unchecked = |field: &'static str, reason: String| JobRejectionError::RecipeUnchecked {
            benchmark: benchmark_id.clone(),
            field,
            reason: bounded(reason),
        };
        let model_json = hf_model
            .or_else(|| spec.get("model"))
            .unwrap_or(&Value::Null);
        let model =
            Model::deserialize(model_json).map_err(|e| unchecked("model", e.to_string()))?;
        // A field this build does not read would reach a client that does: the
        // recipe could only vouch for part of the model.
        let read_back =
            serde_json::to_value(&model).map_err(|e| unchecked("model", e.to_string()))?;
        if without_nulls(&read_back) != without_nulls(model_json) {
            return Err(unchecked(
                "model",
                "it carries a field this build does not read".to_owned(),
            ));
        }
        let model_flags = match spec.get("model_flags") {
            None | Some(Value::Null) => None,
            Some(flags) => Some(
                ModelFlags::deserialize(flags)
                    .map_err(|e| unchecked("model_flags", e.to_string()))?,
            ),
        };
        // Each flag group is the entry's own cell's: flags tagged for another
        // benchmark would run under that cell's rules (the text eval's raw
        // denylist, say) instead of this one's.
        let own_type = entry.benchmark_type.to_string();
        if let Some((field, other)) =
            ["runtime_flags", "benchmark_flags"]
                .into_iter()
                .find_map(|field| {
                    let tag = spec.get(field)?.get("benchmark_type")?;
                    (tag.as_str() != Some(own_type.as_str())).then(|| (field, tag.to_string()))
                })
        {
            return Err(unchecked(field, format!("they are for a {other} cell")));
        }
        let runtime = spec
            .get("runtime")
            .and_then(|runtime| runtime.get("type"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(asr) = catalog
            .get(&benchmark_id)
            .and_then(|benchmark| benchmark.def.as_asr_eval().ok())
            && let Ok(runtime_type) = RuntimeType::deserialize(&runtime)
        {
            asr.check_runtime(runtime_type).map_err(|reason| {
                JobRejectionError::AsrEvalRuntime {
                    benchmark: benchmark_id.clone(),
                    runtime: runtime_type.to_string(),
                    reason: Box::new(reason),
                }
            })?;
        }
        if matches!(model, Model::Torch(_))
            && RuntimeType::deserialize(&runtime).ok() != Some(RuntimeType::UvVllm)
        {
            return Err(JobRejectionError::VlmEvalTorchRuntime {
                benchmark: benchmark_id.clone(),
                runtime: bounded(runtime.to_string()),
            });
        }
        if let Some(flags) = &model_flags
            && !flags.matches(entry.benchmark_type, &model)
        {
            let (benchmark, model) = flags.axes();
            return Err(unchecked(
                "model_flags",
                format!("they are for a {benchmark} cell on a {model} model"),
            ));
        }
        recipe
            .check_cell(
                entry.benchmark_type,
                &model,
                model_flags.as_ref().and_then(ModelFlags::enable_thinking),
            )
            .map_err(|reason| JobRejectionError::OffRecipe {
                benchmark: benchmark_id.clone(),
                reason: Box::new(reason),
            })?;
        if let Some(other) = other_recipe_covering(catalog, &benchmark_id, &entry, &model) {
            return Err(JobRejectionError::AmbiguousRecipe(Box::new(
                AmbiguousRecipeError {
                    repo: hub_repo(&model)
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    task: entry.task.to_owned(),
                    dataset_version: entry.dataset_version.to_owned(),
                    benchmark: benchmark_id.clone(),
                    other: other.clone(),
                },
            )));
        }
    }

    // Last, as the only check of a field the server otherwise only forwards.
    // Rejecting here, before placement, keeps `--strict-models` fail-fast.
    if models.strict
        && let Some(model) = hf_model.or_else(|| spec.get("model"))
        && !models.resolves(model)
    {
        return Err(JobRejectionError::UncataloguedModel {
            // Token-free, like the warning: this message reaches the caller and
            // the log.
            model: bounded(without_auth_tokens(model).to_string()),
        });
    }
    Ok(())
}

/// The recipe of the vlm_eval or asr_eval entry `body`'s spec names, if it
/// names one.
fn recipe_of<'a>(
    catalog: &'a HashMap<BenchmarkId, Benchmark>,
    body: &Value,
) -> Option<&'a pipette_plan_types::VlmEvalRecipe> {
    let id = body.get("spec")?.get("benchmark")?.as_str()?;
    let id = BenchmarkId::try_new(id).ok()?;
    RecipeEntry::of(&catalog.get(&id)?.def).map(|entry| entry.recipe)
}

/// What the recipe check reads off an entry that carries a recipe: a
/// vlm_eval's or an asr_eval's.
struct RecipeEntry<'a> {
    benchmark_type: BenchmarkType,
    task: &'a str,
    dataset_version: &'a str,
    recipe: &'a pipette_plan_types::VlmEvalRecipe,
}

impl<'a> RecipeEntry<'a> {
    fn of(def: &'a pipette_plan_types::BenchmarkDefinition) -> Option<Self> {
        if let Ok(vlm) = def.as_vlm_eval() {
            return Some(Self {
                benchmark_type: BenchmarkType::VlmEval,
                task: vlm.parameter_lmms_task.as_str(),
                dataset_version: &vlm.parameter_dataset_version,
                recipe: vlm.recipe.as_ref()?,
            });
        }
        let asr = def.as_asr_eval().ok()?;
        Some(Self {
            benchmark_type: BenchmarkType::AsrEval,
            task: asr.parameter_asr_task.as_str(),
            dataset_version: &asr.parameter_dataset_version,
            recipe: asr.recipe.as_ref()?,
        })
    }
}

/// The Hub repo a vlm_eval or asr_eval model's files come from, if any: a
/// gguf's, or a torch model's at the repo's root.
fn hub_repo(model: &Model) -> Option<&pipette_plan_types::HfRepo> {
    match model {
        Model::GgufVision(GgufVision {
            source: GgufVisionSource::HuggingFace(files),
        }) => Some(&files.repo),
        Model::Torch(Torch {
            source: ModelSource::HuggingFace { repo, prefix: None },
        }) => Some(repo),
        Model::GgufVision(_)
        | Model::Torch(_)
        | Model::GgufText(_)
        | Model::GgufVisionSpeculative(_)
        | Model::Mlx(_)
        | Model::MlxSpeculative(_)
        | Model::TorchSpeculative(_)
        | Model::Openvino(_)
        | Model::OnnxGenai(_)
        | Model::AppleFoundationText => None,
    }
}

/// Another entry of `entry`'s type for its task and dataset version whose
/// recipe also covers `model`, smallest id first so the message is stable.
/// Only an entry a filer of this one could also name counts: a public one, or
/// one private to the same organization, so no organization's private catalog
/// makes another's jobs ambiguous.
fn other_recipe_covering<'a>(
    catalog: &'a HashMap<BenchmarkId, Benchmark>,
    id: &BenchmarkId,
    entry: &RecipeEntry<'_>,
    model: &Model,
) -> Option<&'a BenchmarkId> {
    let repo = hub_repo(model)?;
    let org = catalog
        .get(id)
        .and_then(|benchmark| benchmark.org_id.as_ref());
    catalog
        .iter()
        .filter(|(other, _)| *other != id)
        .filter(|(_, benchmark)| benchmark.org_id.is_none() || benchmark.org_id.as_ref() == org)
        .filter_map(|(other, benchmark)| Some((other, RecipeEntry::of(&benchmark.def)?)))
        .filter(|(_, other)| {
            other.benchmark_type == entry.benchmark_type
                && other.task == entry.task
                && other.dataset_version == entry.dataset_version
                && if matches!(model, Model::Torch(_)) {
                    other.recipe.covers_own_weights(repo)
                } else {
                    other.recipe.covers(repo)
                }
        })
        .map(|(other, _)| other)
        .min()
}

/// A caller-supplied value cut to a bounded prefix, on a character boundary: a
/// rejection is echoed in a `400` body and on a terminal, and must not drag a
/// megabyte along.
fn bounded(text: String) -> String {
    const MAX_CHARS: usize = 120;
    match text.char_indices().nth(MAX_CHARS) {
        None => text,
        Some((cut, _)) => {
            let mut text = text;
            text.truncate(cut);
            text.push('…');
            text
        }
    }
}

/// The reserved namespace a canonical flag belongs to (the token before the
/// first `:`), or `None` for a free-form flag. Assumes `flag` is canonical,
/// which validation checks first.
fn reserved_namespace(flag: &str) -> Option<&str> {
    if client::is_reserved_capability(flag) {
        flag.split_once(':').map(|(ns, _)| ns)
    } else {
        None
    }
}

/// The eligibility-presence gate: whether a `requires` or `clients` field
/// declares at least one entry.
fn non_empty_array(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// The flat flags of `field`, empty when absent or malformed (shape is validated
/// separately).
fn flat_flags<'a>(body: &'a Value, field: &str) -> impl Iterator<Item = &'a str> {
    body.get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// The clause groups of `field` — one iterator of flags per group — empty when
/// the field is absent or malformed.
fn flag_groups<'a>(
    body: &'a Value,
    field: &str,
) -> impl Iterator<Item = impl Iterator<Item = &'a str>> {
    body.get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .map(|group| group.iter().filter_map(Value::as_str))
}

/// The reserved namespace that makes one of the body's `not_all_of` groups
/// vacuous: a client derives one flag per reserved namespace, so a group that
/// names two excludes nobody, which fails open.
pub(crate) fn vacuous_exclusion_namespace(body: &Value) -> Option<&str> {
    flag_groups(body, "not_all_of").find_map(vacuous_namespace)
}

/// The reserved namespace that makes an exclusion group vacuous: the first one
/// two *distinct* flags of the group share. A repeated flag is the same
/// requirement twice, not a contradiction, so the group is deduplicated first.
fn vacuous_namespace<'a>(group: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = BTreeSet::new();
    let mut namespaces = BTreeSet::new();
    group
        .filter(|flag| seen.insert(*flag))
        .filter_map(reserved_namespace)
        .find(|ns| !namespaces.insert(*ns))
}

/// Every capability flag in a job body — the flat `requires` and `excludes`
/// plus every `any_of` and `not_all_of` group member. `clients` and
/// `exclude_clients` entries are ids, not flags, and are excluded.
fn capability_flags(body: &Value) -> impl Iterator<Item = &str> {
    flat_flags(body, "requires")
        .chain(flat_flags(body, "excludes"))
        .chain(flag_groups(body, "any_of").flatten())
        .chain(flag_groups(body, "not_all_of").flatten())
}

/// Take [`REQUIRED_SUFFIX`] off each routing flag of `jobs` that carries it,
/// before any check reads them, and list the plain flags in the body's
/// [`REQUIRED_FLAGS`].
fn lift_required(jobs: &mut [(String, Value)]) -> Result<(), RejectionError> {
    jobs.iter_mut()
        .enumerate()
        .try_for_each(|(index, (label, body))| {
            lift_required_in(body).map_err(|kind| RejectionError::Job {
                index,
                label: label.clone(),
                kind,
            })
        })
}

/// The flag `marked` marks [`REQUIRED_SUFFIX`]. Only a routing flag takes the
/// mark: any other is measured already, and stays the flag it was.
fn required_mark(marked: &str) -> Option<&str> {
    marked
        .strip_suffix(REQUIRED_SUFFIX)
        .filter(|plain| matching::is_routing_flag(plain))
}

fn lift_required_in(body: &mut Value) -> Result<(), JobRejectionError> {
    // `validate_job` refuses a body that is not an object.
    let Some(fields) = body.as_object_mut() else {
        return Ok(());
    };
    if fields.contains_key(REQUIRED_FLAGS) {
        return Err(JobRejectionError::PresetRequiredFlags);
    }
    let marked = |flag: &Value| flag.as_str().and_then(required_mark).is_some();
    let misplaced = ["excludes", "not_all_of", "clients", "exclude_clients"]
        .into_iter()
        .find_map(|field| {
            let flag = fields
                .get(field)
                .and_then(Value::as_array)?
                .iter()
                .flat_map(|value| match value.as_array() {
                    Some(group) => group.iter().collect(),
                    None => vec![value],
                })
                .find(|flag| marked(flag))?;
            Some((field, bounded(flag.as_str()?.to_string())))
        });
    if let Some((field, flag)) = misplaced {
        return Err(JobRejectionError::RequiredOutsidePositives { field, flag });
    }
    let mut required = BTreeSet::new();
    let mut lift = |flag: &mut Value| -> Result<(), JobRejectionError> {
        let Some(plain) = flag.as_str().and_then(required_mark) else {
            return Ok(());
        };
        if plain.ends_with(':') {
            return Err(JobRejectionError::BareRequiredSuffix {
                flag: bounded(plain.to_string()),
            });
        }
        let plain = plain.to_string();
        required.insert(plain.clone());
        *flag = Value::String(plain);
        Ok(())
    };
    if let Some(flags) = fields.get_mut("requires").and_then(Value::as_array_mut) {
        flags.iter_mut().try_for_each(&mut lift)?;
    }
    if let Some(groups) = fields.get_mut("any_of").and_then(Value::as_array_mut) {
        groups
            .iter_mut()
            .filter_map(Value::as_array_mut)
            .try_for_each(|group| {
                let count = group.iter().filter(|flag| marked(flag)).count();
                if count != 0 && count != group.len() {
                    return Err(JobRejectionError::PartlyRequiredGroup);
                }
                group.iter_mut().try_for_each(&mut lift)
            })?;
    }
    if !required.is_empty() {
        fields.insert(
            REQUIRED_FLAGS.to_string(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    Ok(())
}

/// The handoff `expires_at` grammar: an RFC 3339 timestamp, or `never`. Shared
/// with [`resolve_expires_at`], so every value that validates also resolves.
fn parse_expires_at(s: &str) -> Option<ExpiresAt> {
    match s {
        "never" => Some(ExpiresAt::Never),
        timestamp => DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .map(|dt| ExpiresAt::At(dt.with_timezone(&Utc))),
    }
}

/// Resolve a job's expiry: the handoff's `expires_at` when it carries one, else
/// the ingestion default.
fn resolve_expires_at(body: &Value, default: ExpiresAt) -> ExpiresAt {
    body.get("expires_at")
        .and_then(Value::as_str)
        .and_then(parse_expires_at)
        .unwrap_or(default)
}

/// The requirement identity a fleet-match report groups on: all six eligibility
/// clauses, as sets. Ingestion's frozen warnings and `queue-maintenance`'s
/// `starved` snapshot share it, so an operator can compare the two.
#[derive(PartialEq, Eq)]
pub(crate) struct RequirementKey {
    requires: BTreeSet<String>,
    any_of: BTreeSet<BTreeSet<String>>,
    clients: BTreeSet<String>,
    exclude_clients: BTreeSet<String>,
    excludes: BTreeSet<String>,
    not_all_of: BTreeSet<BTreeSet<String>>,
}

impl RequirementKey {
    /// Render this key as a [`StarvedGroup`] over the jobs that share it.
    pub(crate) fn into_starved_group(self, job_ids: Vec<JobId>) -> StarvedGroup {
        let flatten = |groups: BTreeSet<BTreeSet<String>>| {
            groups
                .into_iter()
                .map(|group| group.into_iter().collect())
                .collect()
        };
        StarvedGroup {
            clauses: pipette_plan_types::StarvedClauses {
                requires: self.requires.into_iter().collect(),
                any_of: flatten(self.any_of),
                clients: self.clients.into_iter().collect(),
                exclude_clients: self.exclude_clients.into_iter().collect(),
                excludes: self.excludes.into_iter().collect(),
                not_all_of: flatten(self.not_all_of),
            },
            job_ids,
        }
    }
}

pub(crate) fn requirement_key(body: &Value) -> RequirementKey {
    let flat = |field| flat_flags(body, field).map(str::to_string).collect();
    let grouped = |field| {
        flag_groups(body, field)
            .map(|group| group.map(str::to_string).collect())
            .collect()
    };
    RequirementKey {
        requires: flat("requires"),
        any_of: grouped("any_of"),
        clients: flat("clients"),
        exclude_clients: flat("exclude_clients"),
        excludes: flat("excludes"),
        not_all_of: grouped("not_all_of"),
    }
}

/// Group the unmatched jobs by identical requirement set into per-group
/// [`Warning`]s, in first-appearance order.
fn group_warnings(unmatched: &[&Placement]) -> Vec<Warning> {
    let mut groups: Vec<(RequirementKey, Vec<JobId>)> = Vec::new();
    for s in unmatched {
        let key = requirement_key(&s.body);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, ids)) => ids.push(s.job_id.clone()),
            None => groups.push((key, vec![s.job_id.clone()])),
        }
    }
    groups
        .into_iter()
        .map(|(key, job_ids)| {
            let group = key.into_starved_group(job_ids);
            Warning {
                kind: WarningKind::Fleet,
                message: warning_message(&group),
                job_ids: group.job_ids,
            }
        })
        .collect()
}

/// Grouped by the canonical `spec.model`, not the name the match reads: a
/// model with no name still needs a line of its own.
fn uncatalogued_model_warnings(models: ModelCheck<'_>, distinct: &[&Placement]) -> Vec<Warning> {
    distinct
        .iter()
        .filter_map(|placement| {
            let model = match &placement.hf_model {
                Some(model) => model,
                None => placement.body.get("spec")?.get("model")?,
            };
            (!models.resolves(model)).then(|| {
                // No token: this string is frozen onto the manifest and
                // returned to the caller.
                (
                    pipette_plan_types::canonical_json(&without_auth_tokens(model)),
                    placement.job_id.clone(),
                )
            })
        })
        // A `Vec` keyed by linear search rather than a map: first-appearance
        // order is the report's order, and a plan names a handful of models.
        .fold(
            Vec::<(String, Vec<JobId>)>::new(),
            |mut groups, (key, job_id)| {
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, ids)) => ids.push(job_id),
                    None => groups.push((key, vec![job_id])),
                }
                groups
            },
        )
        .into_iter()
        .map(|(model, job_ids)| Warning {
            kind: WarningKind::UncataloguedModel,
            message: uncatalogued_model_message(&model, job_ids.len()),
            job_ids,
        })
        .collect()
}

/// The sentence an uncatalogued-model warning carries. It names the
/// consequence: the jobs still run, and their rows lose the parameter columns.
fn uncatalogued_model_message(model: &str, count: usize) -> String {
    let (noun, verb, possessive) = if count == 1 {
        ("job", "names", "its")
    } else {
        ("jobs", "name", "their")
    };
    format!(
        "{count} {noun} {verb} a model with no parameter-count entry ({model}); {possessive} rows \
         will carry whatever counts the client reports, and can be repaired with \
         `fix-model-param` once the table has it"
    )
}

/// The sentence a fleet-match warning carries, rendered by
/// [`describe_clauses`] as `plans show` renders the live starved list.
fn warning_message(group: &StarvedGroup) -> String {
    unmatched_warning(group.job_ids.len(), Some(&describe_clauses(&group.clauses)))
}

/// Serialize the `(label, job_id)` pairs as a JSON object in input order, so
/// cardinal plan keys stay `0`, `1`, … rather than sorting lexically.
pub fn serialize_jobs_map<S: Serializer>(
    jobs: &[(String, JobId)],
    s: S,
) -> Result<S::Ok, S::Error> {
    let mut map = s.serialize_map(Some(jobs.len()))?;
    jobs.iter()
        .try_for_each(|(label, job_id)| map.serialize_entry(label, job_id))?;
    map.end()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::client::{Client, ClientStatus};
    use pipette_core::config::{Config, StorageConfig};
    use pipette_core::denial::DenialRecord;
    use pipette_plan_types::{ClientId, DeviceProfile, UserId};
    use pipette_store::{ClaimResult, Stores, drain_plans};
    use pipette_store_fs::build_local_fs_stores;
    use pipette_store_testkit::{
        assert_each_job_has_its_source_and_link, finish_job, finish_job_with_failure,
        finish_job_with_success, finish_job_with_success_on_build, local_fs_job_record_count,
    };

    use super::*;

    const USER_ADA: &str = "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
    use pipette_plan_types::ContactEmail;
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::PublicKeyHex;

    /// The minimum run specification validation accepts: `benchmark` resolvable
    /// against [`catalog`], plus `model` and `runtime` present. Empty, they do
    /// not parse, so the server reads the spec as opaque.
    fn spec_bench_1() -> Value {
        json!({"benchmark": "bench-1", "model": {}, "runtime": {}})
    }

    fn gguf_text() -> Value {
        json!({"type": "gguf_text", "source": "huggingface", "org": "o", "repo_name": "r", "path": "m.gguf"})
    }

    fn mlx() -> Value {
        json!({"type": "mlx", "source": "huggingface", "org": "o", "repo_name": "r"})
    }

    /// A llama.cpp CLI eval entry for `model_type`.
    fn cli_flags(model_type: &str) -> Value {
        json!({"benchmark_type": "eval", "runtime_type": "llamacpp_cli_stock_tools", "model_type": model_type})
    }

    /// A compatible pair whose model and benchmark flags are for another cell.
    fn foreign_model_and_benchmark_flags() -> Value {
        let mut spec = cli_spec(gguf_text(), Some(cli_flags("gguf_text")));
        if let Some(fields) = spec.as_object_mut() {
            fields.insert(
                "model_flags".to_owned(),
                json!({"benchmark_type": "eval", "model_type": "torch"}),
            );
            fields.insert(
                "benchmark_flags".to_owned(),
                json!({"benchmark_type": "vl_throughput", "runtime_type": "llamacpp_cli_stock_tools", "model_type": "gguf_vision"}),
            );
        }
        spec
    }

    fn flags_for_no_cell() -> Value {
        cli_spec(
            gguf_text(),
            Some(
                json!({"benchmark_type": "eval", "runtime_type": "apple_foundation", "model_type": "gguf_text"}),
            ),
        )
    }

    /// A spec this build parses: `model` on the llama.cpp CLI, for `bench-1`.
    fn cli_spec(model: Value, runtime_flags: Option<Value>) -> Value {
        json!({
            "benchmark": "bench-1",
            "model": model,
            "runtime": {
                "type": "llamacpp_cli_stock_tools", "source": "github_release",
                "repository_url": "github.com/ggml-org/llama.cpp",
                "repository_version": "b5000", "flavor": "macos-arm64",
            },
            "runtime_flags": runtime_flags,
        })
    }

    /// Six hours, the config default. A test that needs the other side of the
    /// gate passes its own.
    const CREATING_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(21_600);
    const MAX_JOBS: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(5000) {
        Some(n) => n,
        None => std::num::NonZeroUsize::MIN,
    };

    /// A one-benchmark catalog keyed by `bench-1`; validation only ever asks it
    /// `contains_key`.
    fn catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let def = "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 100";
        let bench = Benchmark::from_toml("bench-1", def)?;
        Ok(HashMap::from([(BenchmarkId::try_new("bench-1")?, bench)]))
    }

    /// Validate `body` against [`catalog`], with no model check.
    fn check(body: &Value) -> anyhow::Result<Result<(), JobRejectionError>> {
        Ok(validate_job(
            body,
            None,
            &catalog()?,
            ModelCheck::none(),
            Utc::now(),
        ))
    }

    // ── validation: rejections ──────────────────────────────────────────────

    /// A rejection-class expectation. A fn pointer rather than an expected value
    /// because the variants carry payloads that do not compare by equality.
    type Kind = fn(&JobRejectionError) -> bool;

    /// Each expectation is a pattern, never a message, so rewording a variant
    /// cannot make a case pass for the wrong reason.
    #[rstest]
    #[case::not_object(json!("nope"), (|k| matches!(k, JobRejectionError::NotAnObject)) as Kind)]
    #[case::carries_job_id(json!({"requires":["os:macos"],"spec":spec_bench_1(),"job_id":"job-x"}), (|k| matches!(k, JobRejectionError::PresetJobId)) as Kind)]
    #[case::carries_plan_id(json!({"requires":["os:macos"],"spec":spec_bench_1(),"plan_id":"plan-x"}), (|k| matches!(k, JobRejectionError::PresetPlanId)) as Kind)]
    #[case::no_eligibility(json!({"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NoEligibility)) as Kind)]
    #[case::empty_requires_and_clients(json!({"requires":[],"clients":[],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NoEligibility)) as Kind)]
    // A scalar `requires` declares eligibility in the wrong shape, so it reports
    // the shape class rather than absent eligibility.
    #[case::scalar_requires(json!({"requires":"os:macos","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    // `any_of` is a capability clause, so a malformed one is a shape fault even
    // when the body declares no eligibility at all.
    #[case::malformed_any_of_only(json!({"any_of":"x","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    // Both shape checks fire; the clause classifier runs first, which pins their
    // relative order.
    #[case::both_fields_malformed(json!({"requires":"a","clients":"b","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    // An array holding a non-string clears the eligibility check, so this is the
    // shape classifier's own rejection.
    #[case::non_string_requires(json!({"requires":["os:macos",42],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    #[case::malformed_any_of(json!({"requires":["os:macos"],"any_of":"x","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    // The offending flag is carried, not described, so a caller can name it.
    #[case::non_canonical_flag(json!({"requires":["OS:macos"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NonCanonicalFlag { flag } if flag == "OS:macos")) as Kind)]
    #[case::non_canonical_any_of_flag(json!({"requires":["os:ios"],"any_of":[["OS:macos"]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NonCanonicalFlag { flag } if flag == "OS:macos")) as Kind)]
    #[case::whitespace_flag(json!({"requires":["os:mac os"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NonCanonicalFlag { .. })) as Kind)]
    #[case::reserved_namespace_dup(json!({"requires":["os:ios","os:android"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::DuplicateReservedNamespace { namespace } if namespace == "os")) as Kind)]
    #[case::malformed_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"soon"}), (|k| matches!(k, JobRejectionError::MalformedExpiresAt { value } if value == "\"soon\"")) as Kind)]
    // A non-string `expires_at` is the same class, and the carried JSON tells it
    // from the unparseable-string case above.
    #[case::non_string_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":42}), (|k| matches!(k, JobRejectionError::MalformedExpiresAt { value } if value == "42")) as Kind)]
    // The rejection echoes the offending value into a `400` body, so a job body
    // that carries a huge one is cut to a bounded prefix.
    #[case::oversized_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"x".repeat(5000)}), (|k| matches!(k, JobRejectionError::MalformedExpiresAt { value } if value.chars().count() == 121 && value.ends_with('…'))) as Kind)]
    #[case::past_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"2020-01-01T00:00:00Z"}), (|k| matches!(k, JobRejectionError::PastExpiresAt { value } if value == "2020-01-01T00:00:00Z")) as Kind)]
    // RFC 3339 allows any number of fractional digits, so a past value can be
    // huge too.
    #[case::oversized_past_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":format!("2020-01-01T00:00:00.{}Z", "0".repeat(5000))}), (|k| matches!(k, JobRejectionError::PastExpiresAt { value } if value.chars().count() == 121 && value.ends_with('…'))) as Kind)]
    #[case::string_merge_duplicates(json!({"requires":["os:macos"],"spec":spec_bench_1(),"merge_duplicates":"false"}), (|k| matches!(k, JobRejectionError::MalformedMergeDuplicates { value } if value == "\"false\"")) as Kind)]
    #[case::numeric_merge_duplicates(json!({"requires":["os:macos"],"spec":spec_bench_1(),"merge_duplicates":0}), (|k| matches!(k, JobRejectionError::MalformedMergeDuplicates { value } if value == "0")) as Kind)]
    #[case::clients_not_an_array(json!({"requires":["os:macos"],"clients":"ev1_a","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::ClientsNotAnArray)) as Kind)]
    // A scalar `clients` is a shape fault whether or not the body also declares a
    // `requires`.
    #[case::scalar_clients_only(json!({"clients":"ev1_a","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::ClientsNotAnArray)) as Kind)]
    #[case::non_string_clients(json!({"clients":["ev1_a",7],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::ClientsNotStrings)) as Kind)]
    #[case::missing_spec(json!({"requires":["os:macos"]}), (|k| matches!(k, JobRejectionError::MissingSpec)) as Kind)]
    #[case::spec_not_object(json!({"requires":["os:macos"],"spec":"eval_test"}), (|k| matches!(k, JobRejectionError::SpecNotAnObject)) as Kind)]
    #[case::missing_benchmark(json!({"requires":["os:macos"],"spec":{"model":{},"runtime":{}}}), (|k| matches!(k, JobRejectionError::MissingBenchmark)) as Kind)]
    // A non-string `benchmark` is the same class as an absent one — the variant
    // message says "missing or not a string".
    #[case::non_string_benchmark(json!({"requires":["os:macos"],"spec":{"benchmark":42,"model":{},"runtime":{}}}), (|k| matches!(k, JobRejectionError::MissingBenchmark)) as Kind)]
    // A benchmark id outside the allowed charset is malformed, which is a
    // different answer from a well-formed id the catalog does not hold.
    #[case::invalid_benchmark_id(json!({"requires":["os:macos"],"spec":{"benchmark":"bench 1","model":{},"runtime":{}}}), (|k| matches!(k, JobRejectionError::InvalidBenchmarkId(_))) as Kind)]
    #[case::unknown_benchmark(json!({"requires":["os:macos"],"spec":{"benchmark":"nope","model":{},"runtime":{}}}), (|k| matches!(k, JobRejectionError::UnknownBenchmark { id, .. } if id.as_str() == "nope")) as Kind)]
    #[case::missing_model(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","runtime":{}}}), (|k| matches!(k, JobRejectionError::MissingSpecField { field: "model" })) as Kind)]
    #[case::missing_runtime(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":{}}}), (|k| matches!(k, JobRejectionError::MissingSpecField { field: "runtime" })) as Kind)]
    #[case::null_model(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":null,"runtime":{}}}), (|k| matches!(k, JobRejectionError::MissingSpecField { field: "model" })) as Kind)]
    #[case::null_runtime(json!({"requires":["os:macos"],"spec":{"benchmark":"bench-1","model":{},"runtime":null}}), (|k| matches!(k, JobRejectionError::MissingSpecField { field: "runtime" })) as Kind)]
    #[case::incompatible_pair(json!({"requires":["os:macos"],"spec":cli_spec(mlx(), None)}), (|k| matches!(k, JobRejectionError::UnrunnableSpec(RunSpecError::Incompatible { .. }))) as Kind)]
    #[case::incompatible_pair_with_its_flags(json!({"requires":["os:macos"],"spec":cli_spec(mlx(), Some(cli_flags("gguf_text")))}), (|k| matches!(k, JobRejectionError::UnrunnableSpec(RunSpecError::Incompatible { .. }))) as Kind)]
    #[case::runtime_flags_for_another_runtime(json!({"requires":["os:macos"],"spec":cli_spec(gguf_text(), Some(json!({"benchmark_type":"prefill_throughput","runtime_type":"uv_openvino","model_type":"openvino"})))}), (|k| matches!(k, JobRejectionError::UnrunnableSpec(RunSpecError::RuntimeFlagsForAnotherPair { .. }))) as Kind)]
    #[case::runtime_flags_for_another_model(json!({"requires":["os:macos"],"spec":cli_spec(gguf_text(), Some(json!({"benchmark_type":"vl_throughput","runtime_type":"llamacpp_cli_stock_tools","model_type":"gguf_vision"})))}), (|k| matches!(k, JobRejectionError::UnrunnableSpec(RunSpecError::RuntimeFlagsForAnotherPair { .. }))) as Kind)]
    // A body from a writer predating the envelope split: the spec content is
    // flat, so there is no `spec` to lease out and the job would be unrunnable.
    #[case::pre_envelope_flat_body(json!({"requires":["os:macos"],"benchmark_id":"bench-1","model_descriptor":"{}"}), (|k| matches!(k, JobRejectionError::MissingSpec)) as Kind)]
    // The negatives join the shape classifier, so a typo there is an ingest
    // error rather than a job that silently matches nobody.
    #[case::malformed_excludes(json!({"requires":["os:macos"],"excludes":"chip:a15","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    #[case::malformed_not_all_of(json!({"requires":["os:macos"],"not_all_of":["chip:a15"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    #[case::malformed_exclude_clients(json!({"clients":["ev1_a"],"exclude_clients":"ev1_b","spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    #[case::non_string_exclude_clients(json!({"clients":["ev1_a"],"exclude_clients":["ev1_b",7],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::MalformedCapabilityClauses)) as Kind)]
    // The canonical check covers the negatives too: a non-canonical exclusion
    // flag can never match, so the exclusion would be silently absent.
    #[case::non_canonical_exclude_flag(json!({"requires":["os:ios"],"excludes":["CHIP:A15"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NonCanonicalFlag { flag } if flag == "CHIP:A15")) as Kind)]
    #[case::non_canonical_not_all_of_flag(json!({"requires":["os:ios"],"not_all_of":[["os:ios","CHIP:A15"]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::NonCanonicalFlag { flag } if flag == "CHIP:A15")) as Kind)]
    // A `not_all_of` group that names two flags of one reserved namespace cannot
    // be carried whole, so it excludes nobody.
    #[case::vacuous_exclusion_group(json!({"requires":["runtime:mlx"],"not_all_of":[["os:ios","os:android"]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::VacuousExclusionGroup { namespace } if namespace == "os")) as Kind)]
    // Bodies no possible client can match, one case per rule.
    #[case::roster_excluded_away(json!({"clients":["ev1_a"],"exclude_clients":["ev1_a"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    #[case::required_flag_excluded(json!({"requires":["os:ios"],"excludes":["os:ios"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    #[case::not_all_of_inside_requires(json!({"requires":["os:ios","runtime:mlx"],"not_all_of":[["os:ios","runtime:mlx"]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    #[case::any_of_group_wholly_excluded(json!({"requires":["os:ios"],"any_of":[["chip:a15","chip:a16"]],"excludes":["chip:a15","chip:a16"],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    #[case::empty_not_all_of_group(json!({"requires":["os:ios"],"not_all_of":[[]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    #[case::empty_not_all_of_group_with_roster(json!({"clients":["ev1_a"],"not_all_of":[[]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    // An `any_of` group with no members is an at-least-one-of over nothing.
    #[case::empty_any_of_group(json!({"requires":["os:ios"],"any_of":[[]],"spec":spec_bench_1()}), (|k| matches!(k, JobRejectionError::StaticallyUnmatchable { .. })) as Kind)]
    fn validate_job_rejects(#[case] body: Value, #[case] expected: Kind) -> anyhow::Result<()> {
        let kind = check(&body)?.err().context("body is rejected")?;
        assert!(expected(&kind), "wrong rejection class: {kind:?}");
        Ok(())
    }

    /// An unknown benchmark within a typo of a catalog id names that id; one
    /// far from every id names none.
    #[rstest]
    #[case::typo(
        "bench-2",
        "spec.benchmark bench-2 not in the benchmark catalog; did you mean bench-1?"
    )]
    #[case::far("nope", "spec.benchmark nope not in the benchmark catalog")]
    fn an_unknown_benchmark_suggests_a_near_name(
        #[case] benchmark: &str,
        #[case] message: &str,
    ) -> anyhow::Result<()> {
        let body = json!({"requires": ["os:macos"], "spec": {"benchmark": benchmark, "model": {}, "runtime": {}}});
        let kind = check(&body)?.err().context("body is rejected")?;
        assert_eq!(kind.to_string(), message);
        Ok(())
    }

    // ── validation: an eval cell runs its entry as defined ──────────────────

    /// `eval-ctx` bounds prompt plus output at the output cap; `eval-cap` caps the output alone.
    fn eval_catalog() -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let entry = |extra: &str| {
            format!(
                "benchmark_type = \"eval\"\nparameter_eval_id = \"math_500\"\n\
                 parameter_dataset_name = \"2026.10.1\"\nparameter_max_tokens = 32768\n{extra}"
            )
        };
        [
            ("eval-ctx", entry("parameter_max_context = 32768")),
            ("eval-cap", entry("")),
        ]
        .into_iter()
        .map(|(id, toml)| Ok((BenchmarkId::try_new(id)?, Benchmark::from_toml(id, &toml)?)))
        .collect()
    }

    fn torch() -> Value {
        json!({"type": "torch", "source": "huggingface", "org": "o", "repo_name": "r"})
    }

    fn uv_vllm() -> Value {
        json!({
            "type": "uv_vllm", "server_version": "0.31.0", "build": "cu128",
            "python_version": "3.12",
            "source": {"type": "pip_requirements_text", "contents": "vllm==0.31.0"},
        })
    }

    fn docker_sglang() -> Value {
        json!({
            "type": "docker_sglang", "image_name": "lmsysorg/sglang",
            "image_tag": "v0.5.15-cu130", "flavor": "nvidia_gpu",
        })
    }

    /// The eval cell's flags on `runtime_type` for `model_type`, with `knobs`.
    fn eval_flags(runtime_type: &str, model_type: &str, knobs: &Value) -> Value {
        let mut flags = json!({"benchmark_type": "eval", "runtime_type": runtime_type, "model_type": model_type});
        if let (Some(flags), Some(knobs)) = (flags.as_object_mut(), knobs.as_object()) {
            flags.extend(knobs.clone());
        }
        flags
    }

    /// A job for `benchmark` on vLLM, with `knobs` as the eval cell's flags.
    fn vllm_job(benchmark: &str, knobs: &Value) -> Value {
        let flags = eval_flags("uv_vllm", "torch", knobs);
        json!({"requires": ["os:ubuntu"], "spec": {"benchmark": benchmark, "model": torch(), "runtime": uv_vllm(), "runtime_flags": flags}})
    }

    /// A job for `benchmark` on llama-server, with `knobs` as the cell's flags.
    fn llama_job(benchmark: &str, knobs: &Value) -> Value {
        let mut job = json!({"requires": ["os:macos"], "spec": cli_spec(gguf_text(), Some(eval_flags("llamacpp_cli_stock_tools", "gguf_text", knobs)))});
        job["spec"]["benchmark"] = json!(benchmark);
        job
    }

    /// Why a cell cannot run its entry. A fn pointer, as [`Kind`] is.
    type ContextKind = fn(&EvalContextError) -> bool;

    /// The wiring: each verdict family reaches the job's rejection. The rules
    /// themselves are tested beside `check_cell_context`.
    #[rstest]
    #[case::bounded_vllm_at_the_bound(vllm_job("eval-ctx", &json!({"max_model_len": 32768})), None)]
    #[case::bounded_vllm_unpinned(vllm_job("eval-ctx", &json!({})), Some((|e| matches!(e, EvalContextError::ContextUnpinned { .. })) as ContextKind))]
    #[case::bounded_llama_one_slot(llama_job("eval-ctx", &json!({"ctx_size": 32768})), None)]
    #[case::bounded_llama_kv_unified(llama_job("eval-ctx", &json!({"ctx_size": 32768, "raw": ["--kv-unified"]})), Some((|e| matches!(e, EvalContextError::RawChangesSlotContext { .. })) as ContextKind))]
    #[case::bounded_sglang(json!({"requires": ["os:ubuntu"], "spec": {"benchmark": "eval-ctx", "model": torch(), "runtime": docker_sglang()}}), Some((|e| matches!(e, EvalContextError::RuntimeCannotBound { runtime: RuntimeType::DockerSglang, .. })) as ContextKind))]
    #[case::unbounded_vllm_no_room(vllm_job("eval-cap", &json!({"max_model_len": 32768})), Some((|e| matches!(e, EvalContextError::NoRoomForPrompt { .. })) as ContextKind))]
    #[case::unbounded_sglang(json!({"requires": ["os:ubuntu"], "spec": {"benchmark": "eval-cap", "model": torch(), "runtime": docker_sglang()}}), None)]
    fn an_eval_cell_runs_its_entry_as_defined(
        #[case] body: Value,
        #[case] expected: Option<ContextKind>,
    ) -> anyhow::Result<()> {
        let checked = validate_job(
            &body,
            None,
            &eval_catalog()?,
            ModelCheck::none(),
            Utc::now(),
        );
        match expected {
            None => anyhow::ensure!(checked.is_ok(), "{checked:?}"),
            Some(expected) => anyhow::ensure!(
                matches!(&checked, Err(JobRejectionError::EvalContext { reason, .. }) if expected(reason)),
                "wrong rejection: {checked:?}"
            ),
        }
        Ok(())
    }

    // ── validation: a vlm_eval cell runs its entry's recipe ─────────────────

    /// Two MMStar entries, Qwen3.5's (thinking on) and LFM2.5-VL's (no
    /// thinking mode), and a POPE one that also covers Qwen3.5-2B, which is
    /// not ambiguous since it is another task.
    fn vlm_eval_catalog(
        extra: &[(&str, &str, &str)],
    ) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let entry = |task: &str, recipe: &str| {
            format!(
                "benchmark_type = \"vlm_eval\"\nparameter_lmms_task = \"{task}\"\n\
                 parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 10\n\
                 parameter_max_tokens = 8192\n\
                 [dataset]\nformat = \"vlm-eval-corpus/1\"\nadapter = \"evals-server@1\"\n\
                 digest = \"{}\"\nbytes = 1024\n\
                 [dataset.upstream]\nsource = \"evals_server\"\neval_id = \"{task}\"\n\
                 dataset_version = \"2026.10.1\"\nidentity = \"{}\"\n\
                 [recipe]\n{recipe}\nsources = [\"https://huggingface.co/x\"]\n",
                "1".repeat(64),
                "2".repeat(64),
            )
        };
        let qwen = "family = \"Qwen3.5\"\nmodels = [{ repo = \"unsloth/Qwen3.5-2B-GGUF\" }, { repo = \"Qwen/Qwen3.5-2B\" }]\nenable_thinking = true";
        let lfm = "family = \"LFM2.5-VL\"\nmodels = [{ repo = \"LiquidAI/LFM2.5-VL-3B-GGUF\" }]";
        [
            ("vlm-mmstar-qwen", "mmstar", qwen),
            ("vlm-mmstar-lfm", "mmstar", lfm),
            ("vlm-pope-qwen", "pope", qwen),
        ]
        .iter()
        .chain(extra)
        .map(|(id, task, recipe)| {
            Ok((
                BenchmarkId::try_new(*id)?,
                Benchmark::from_toml(id, &entry(task, recipe))?,
            ))
        })
        .collect()
    }

    /// A vlm_eval job for `benchmark` running `org/repo` with `thinking`.
    fn vlm_job(benchmark: &str, org: &str, repo: &str, thinking: Option<bool>) -> Value {
        let model = json!({
            "type": "gguf_vision", "source": "huggingface", "org": org, "repo_name": repo,
            "model": "m-Q4_K_M.gguf", "mmproj": "mmproj-F16.gguf",
        });
        let mut job = json!({"requires": ["os:ubuntu"], "spec": cli_spec(model, None)});
        job["spec"]["benchmark"] = json!(benchmark);
        if let Some(thinking) = thinking {
            job["spec"]["model_flags"] = json!({"benchmark_type": "vlm_eval", "model_type": "gguf_vision", "enable_thinking": thinking});
        }
        job
    }

    /// A vlm_eval job for `benchmark` running the torch weights of `org/repo`
    /// on vLLM, with `thinking`.
    fn vlm_vllm_job(benchmark: &str, org: &str, repo: &str, thinking: Option<bool>) -> Value {
        let model =
            json!({"type": "torch", "source": "huggingface", "org": org, "repo_name": repo});
        let flags = json!({"benchmark_type": "vlm_eval", "runtime_type": "uv_vllm", "model_type": "torch", "parallel": 64});
        let mut job = json!({"requires": ["os:ubuntu"], "spec": {"benchmark": benchmark, "model": model, "runtime": uv_vllm(), "runtime_flags": flags}});
        if let Some(thinking) = thinking {
            job["spec"]["model_flags"] = json!({"benchmark_type": "vlm_eval", "model_type": "torch", "enable_thinking": thinking});
        }
        job
    }

    /// Why a cell is off its recipe. A fn pointer, as [`Kind`] is.
    type RecipeKind = fn(&VlmEvalRecipeError) -> bool;

    #[rstest]
    // Any quant of a covered repo, in the recipe's mode.
    #[case::on_recipe(
        vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(true)),
        None
    )]
    #[case::no_thinking_mode_no_flags(
        vlm_job("vlm-mmstar-lfm", "LiquidAI", "LFM2.5-VL-3B-GGUF", None),
        None
    )]
    #[case::another_repo(vlm_job("vlm-mmstar-qwen", "LiquidAI", "LFM2.5-VL-3B-GGUF", None), Some((|e| matches!(e, VlmEvalRecipeError::NotCovered { .. })) as RecipeKind))]
    #[case::thinking_off(vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(false)), Some((|e| matches!(e, VlmEvalRecipeError::Thinking { .. })) as RecipeKind))]
    #[case::thinking_unstated(vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", None), Some((|e| matches!(e, VlmEvalRecipeError::Thinking { .. })) as RecipeKind))]
    #[case::thinking_on_a_family_without_it(vlm_job("vlm-mmstar-lfm", "LiquidAI", "LFM2.5-VL-3B-GGUF", Some(true)), Some((|e| matches!(e, VlmEvalRecipeError::Thinking { .. })) as RecipeKind))]
    // The same rules for a covered repo's own weights on vLLM.
    #[case::torch_on_recipe(
        vlm_vllm_job("vlm-mmstar-qwen", "Qwen", "Qwen3.5-2B", Some(true)),
        None
    )]
    #[case::torch_of_an_uncovered_repo(vlm_vllm_job("vlm-mmstar-lfm", "LiquidAI", "LFM2.5-VL-3B", None), Some((|e| matches!(e, VlmEvalRecipeError::NotCovered { .. })) as RecipeKind))]
    #[case::torch_thinking_off(vlm_vllm_job("vlm-mmstar-qwen", "Qwen", "Qwen3.5-2B", Some(false)), Some((|e| matches!(e, VlmEvalRecipeError::Thinking { .. })) as RecipeKind))]
    #[case::torch_subdirectory(
        {
            let mut job = vlm_vllm_job("vlm-mmstar-qwen", "Qwen", "Qwen3.5-2B", Some(true));
            job["spec"]["model"]["prefix"] = json!("sub");
            job
        },
        Some((|e| matches!(e, VlmEvalRecipeError::Subdirectory { .. })) as RecipeKind)
    )]
    fn a_vlm_eval_cell_runs_its_entrys_recipe(
        #[case] body: Value,
        #[case] expected: Option<RecipeKind>,
    ) -> anyhow::Result<()> {
        let checked = validate_job(
            &body,
            None,
            &vlm_eval_catalog(&[])?,
            ModelCheck::none(),
            Utc::now(),
        );
        match expected {
            None => anyhow::ensure!(checked.is_ok(), "{checked:?}"),
            Some(expected) => anyhow::ensure!(
                matches!(&checked, Err(JobRejectionError::OffRecipe { reason, .. }) if expected(reason)),
                "wrong rejection: {checked:?}"
            ),
        }
        Ok(())
    }

    /// The recipe is read off the model and its flags alone, so a spec whose
    /// runtime this build cannot parse (a newer client's) is held to it too,
    /// and one whose model it cannot read is not queued unchecked.
    #[test]
    fn a_vlm_eval_recipe_holds_whatever_runtime_the_spec_names() -> anyhow::Result<()> {
        let catalog = vlm_eval_catalog(&[])?;
        let mut newer = vlm_job("vlm-mmstar-qwen", "LiquidAI", "LFM2.5-VL-3B-GGUF", None);
        newer["spec"]["runtime"] = json!({"type": "a_runtime_from_a_newer_client"});
        let checked = validate_job(&newer, None, &catalog, ModelCheck::none(), Utc::now());
        anyhow::ensure!(
            matches!(&checked, Err(JobRejectionError::OffRecipe { reason, .. })
                if matches!(**reason, VlmEvalRecipeError::NotCovered { .. })),
            "{checked:?}"
        );
        let mut on_recipe = vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(true));
        on_recipe["spec"]["runtime"] = json!({"type": "a_runtime_from_a_newer_client"});
        validate_job(&on_recipe, None, &catalog, ModelCheck::none(), Utc::now())?;
        let mut unreadable = vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(true));
        unreadable["spec"]["model"] = json!({"type": "a_model_kind_from_a_newer_client"});
        let checked = validate_job(&unreadable, None, &catalog, ModelCheck::none(), Utc::now());
        anyhow::ensure!(
            matches!(
                &checked,
                Err(JobRejectionError::RecipeUnchecked { field: "model", .. })
            ),
            "{checked:?}"
        );
        Ok(())
    }

    /// A repo's own weights run a vlm_eval on uv vLLM alone: the cell another
    /// runtime would name does not exist, so no client could run the job.
    #[rstest]
    #[case::uv_vllm(uv_vllm(), true)]
    #[case::docker_sglang(docker_sglang(), false)]
    #[case::uv_sglang(json!({"type": "uv_sglang", "server_version": "0.5.0", "build": "cu128", "python_version": "3.12", "source": {"type": "pip_requirements_text", "contents": "sglang==0.5.0"}}), false)]
    #[case::docker_vllm(json!({"type": "docker_vllm", "image_name": "vllm/vllm-openai", "image_tag": "v0.31.0", "flavor": "nvidia_gpu"}), false)]
    #[case::unknown(json!({"type": "a_runtime_from_a_newer_client"}), false)]
    fn a_torch_vlm_eval_runs_on_uv_vllm(
        #[case] runtime: Value,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let mut job = vlm_vllm_job("vlm-mmstar-qwen", "Qwen", "Qwen3.5-2B", Some(true));
        job["spec"]["runtime"] = runtime;
        if let Some(spec) = job["spec"].as_object_mut() {
            spec.remove("runtime_flags");
        }
        let checked = validate_job(
            &job,
            None,
            &vlm_eval_catalog(&[])?,
            ModelCheck::none(),
            Utc::now(),
        );
        if accepted {
            checked?;
        } else {
            anyhow::ensure!(
                matches!(&checked, Err(JobRejectionError::VlmEvalTorchRuntime { .. })),
                "{checked:?}"
            );
        }
        Ok(())
    }

    /// A flag group tagged for another benchmark would run under that cell's
    /// rules: a vlm_eval job carries vlm_eval flags, or none.
    #[rstest]
    #[case::runtime_flags("runtime_flags", json!({"benchmark_type": "eval", "runtime_type": "uv_vllm", "model_type": "torch", "raw": ["--generation-config", "auto"]}))]
    #[case::benchmark_flags("benchmark_flags", json!({"benchmark_type": "eval", "runtime_type": "uv_vllm", "model_type": "torch"}))]
    fn a_vlm_eval_job_carries_vlm_eval_flags(
        #[case] field: &'static str,
        #[case] flags: Value,
    ) -> anyhow::Result<()> {
        let mut job = vlm_vllm_job("vlm-mmstar-qwen", "Qwen", "Qwen3.5-2B", Some(true));
        job["spec"][field] = flags;
        let checked = validate_job(
            &job,
            None,
            &vlm_eval_catalog(&[])?,
            ModelCheck::none(),
            Utc::now(),
        );
        anyhow::ensure!(
            matches!(&checked, Err(JobRejectionError::RecipeUnchecked { field: f, .. }) if *f == field),
            "{checked:?}"
        );
        Ok(())
    }

    /// Two entries for one task whose recipes both cover a repo's own weights
    /// make its torch job ambiguous; a line that names a projector covers the
    /// repo's ggufs alone, so it does not.
    #[rstest]
    #[case::both_cover_the_weights("{ repo = \"Qwen/Qwen3.5-2B\" }", true)]
    #[case::a_projector_line("{ repo = \"Qwen/Qwen3.5-2B\", mmproj = \"mmproj-F16.gguf\" }", false)]
    fn a_torch_model_two_recipes_cover_is_ambiguous(
        #[case] line: &str,
        #[case] ambiguous: bool,
    ) -> anyhow::Result<()> {
        let rival = format!("family = \"Rival\"\nmodels = [{line}]\nenable_thinking = true");
        let catalog = vlm_eval_catalog(&[("vlm-mmstar-rival", "mmstar", rival.as_str())])?;
        let job = vlm_vllm_job("vlm-mmstar-qwen", "Qwen", "Qwen3.5-2B", Some(true));
        let checked = validate_job(&job, None, &catalog, ModelCheck::none(), Utc::now());
        if ambiguous {
            anyhow::ensure!(
                matches!(&checked, Err(JobRejectionError::AmbiguousRecipe(_))),
                "{checked:?}"
            );
        } else {
            checked?;
        }
        Ok(())
    }

    /// One organization's private recipe does not make another's ambiguous,
    /// nor a public one's; within one organization it does.
    #[test]
    fn a_private_recipe_is_ambiguous_only_within_its_organization() -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let rival = "family = \"Qwen3.5 rival\"\nmodels = [{ repo = \"unsloth/Qwen3.5-2B-GGUF\" }]\nenable_thinking = true";
        let mut catalog = vlm_eval_catalog(&[("vlm-mmstar-rival", "mmstar", rival)])?;
        let (one, other) = (
            pipette_plan_types::OrgId::mint(),
            pipette_plan_types::OrgId::mint(),
        );
        let private = |catalog: &mut HashMap<BenchmarkId, Benchmark>,
                       id: &str,
                       org: &pipette_plan_types::OrgId|
         -> anyhow::Result<()> {
            let id = BenchmarkId::try_new(id)?;
            let entry = catalog.remove(&id).context("entry")?;
            catalog.insert(id, entry.private_to(org.clone()));
            Ok(())
        };
        private(&mut catalog, "vlm-mmstar-rival", &one)?;
        let job = |id: &str| vlm_job(id, "unsloth", "Qwen3.5-2B-GGUF", Some(true));
        let check = |catalog: &HashMap<BenchmarkId, Benchmark>, id: &str| {
            validate_job(&job(id), None, catalog, ModelCheck::none(), Utc::now())
        };
        // The public entry's filer cannot name the private one.
        check(&catalog, "vlm-mmstar-qwen")?;
        private(&mut catalog, "vlm-mmstar-qwen", &other)?;
        check(&catalog, "vlm-mmstar-qwen")?;
        check(&catalog, "vlm-mmstar-rival")?;
        private(&mut catalog, "vlm-mmstar-qwen", &one)?;
        anyhow::ensure!(
            matches!(
                check(&catalog, "vlm-mmstar-qwen"),
                Err(JobRejectionError::AmbiguousRecipe(_))
            ),
            "two of one organization's entries cover the model"
        );
        Ok(())
    }

    /// The recipe vouches only for what it reads: a model with a field this
    /// build does not know, or flags for another cell, is not queued; an
    /// imported model is judged by its Hugging Face origin.
    #[test]
    fn a_vlm_eval_recipe_reads_the_whole_model_and_its_origin() -> anyhow::Result<()> {
        let catalog = vlm_eval_catalog(&[])?;
        let check = |body: &Value, alias: Option<&Value>| {
            validate_job(body, alias, &catalog, ModelCheck::none(), Utc::now())
        };
        let mut extra = vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(true));
        extra["spec"]["model"]["adapter"] = json!("lora.gguf");
        let checked = check(&extra, None);
        anyhow::ensure!(
            matches!(
                &checked,
                Err(JobRejectionError::RecipeUnchecked { field: "model", .. })
            ),
            "{checked:?}"
        );
        let mut foreign = vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", None);
        foreign["spec"]["model_flags"] =
            json!({"benchmark_type": "eval", "model_type": "torch", "enable_thinking": true});
        let checked = check(&foreign, None);
        anyhow::ensure!(
            matches!(
                &checked,
                Err(JobRejectionError::RecipeUnchecked {
                    field: "model_flags",
                    ..
                })
            ),
            "{checked:?}"
        );
        // A model stored off the Hub, whose origin is a covered repo.
        let mut stored = vlm_job("vlm-mmstar-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(true));
        let origin = stored["spec"]["model"].clone();
        stored["spec"]["model"] = json!({
            "type": "gguf_vision", "source": "relative_files", "model": "m.gguf", "mmproj": "p.gguf",
        });
        anyhow::ensure!(
            check(&stored, None).is_err(),
            "off the Hub without an origin"
        );
        check(&stored, Some(&origin))?;
        Ok(())
    }

    /// Two entries for one task covering one model would give it two scores,
    /// so neither is run until the catalog says which.
    #[test]
    fn a_model_two_recipes_cover_for_one_task_is_refused() -> anyhow::Result<()> {
        let rival = "family = \"Qwen3.5 rival\"\nmodels = [{ repo = \"Unsloth/qwen3.5-2b-gguf\" }]\nenable_thinking = true";
        let catalog = vlm_eval_catalog(&[("vlm-mmstar-rival", "mmstar", rival)])?;
        let checked = validate_job(
            &vlm_job("vlm-mmstar-rival", "unsloth", "Qwen3.5-2B-GGUF", Some(true)),
            None,
            &catalog,
            ModelCheck::none(),
            Utc::now(),
        );
        let Err(rejection) = checked else {
            anyhow::bail!("the job ran under one of two recipes");
        };
        assert_eq!(
            rejection.to_string(),
            "unsloth/Qwen3.5-2B-GGUF is covered by two recipes for mmstar 2026.10.1: \
             vlm-mmstar-rival and vlm-mmstar-qwen"
        );
        // The other task's entry is untouched by it.
        validate_job(
            &vlm_job("vlm-pope-qwen", "unsloth", "Qwen3.5-2B-GGUF", Some(true)),
            None,
            &catalog,
            ModelCheck::none(),
            Utc::now(),
        )?;
        Ok(())
    }

    // ── validation: an asr_eval cell runs its entry's recipe ────────────────

    /// Voxtral's FLEURS Dutch entry, and Gemma 4's, both private to `org` as
    /// every asr_eval entry is; and `extra` (id, task, recipe) entries public.
    fn asr_eval_catalog(
        org: &pipette_plan_types::OrgId,
        extra: &[(&str, &str, &str)],
    ) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let entry = |task: &str, recipe: &str| {
            format!(
                "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"{task}\"\n\
                 parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 364\n\
                 parameter_prompt_profile = \"voxtral\"\nparameter_max_tokens = 500\n\
                 [dataset]\nformat = \"asr-eval-corpus/1\"\nadapter = \"evals-server@1\"\n\
                 digest = \"{}\"\nbytes = 1024\n\
                 [dataset.upstream]\nsource = \"evals_server\"\neval_id = \"{task}\"\n\
                 dataset_version = \"2026.10.1\"\nprompt_profile = \"voxtral\"\nidentity = \"{}\"\n\
                 [recipe]\n{recipe}\nenable_thinking = false\nsources = [\"https://huggingface.co/x\"]\n",
                "1".repeat(64),
                "2".repeat(64),
            )
        };
        let voxtral = "family = \"Voxtral\"\nmodels = [{ repo = \"ggml-org/Voxtral-Mini-3B-2507-GGUF\" }, { repo = \"bartowski/mistralai_Voxtral-Mini-3B-2507-GGUF\" }, { repo = \"mistralai/Voxtral-Mini-3B-2507\" }]";
        let gemma = "family = \"Gemma 4\"\nmodels = [{ repo = \"ggml-org/gemma-4-E2B-it-GGUF\" }]";
        let private = [
            ("asr-nl-voxtral", "asr_fleurs_nl_nl", voxtral),
            ("asr-nl-gemma", "asr_fleurs_nl_nl", gemma),
            ("asr-de-voxtral", "asr_fleurs_de_de", voxtral),
        ];
        let mut catalog = HashMap::new();
        for (id, task, recipe) in private {
            catalog.insert(
                BenchmarkId::try_new(id)?,
                Benchmark::from_toml(id, &entry(task, recipe))?.private_to(org.clone()),
            );
        }
        for (id, task, recipe) in extra {
            catalog.insert(
                BenchmarkId::try_new(*id)?,
                Benchmark::from_toml(id, &entry(task, recipe))?,
            );
        }
        Ok(catalog)
    }

    /// An asr_eval job for `benchmark` running `org/repo`, its model flags
    /// stating `thinking` (none when `None`), for the `benchmark_type` cell.
    fn asr_job(
        benchmark: &str,
        org: &str,
        repo: &str,
        thinking: Option<bool>,
        flags_for: &str,
    ) -> Value {
        let model = json!({
            "type": "gguf_vision", "source": "huggingface", "org": org, "repo_name": repo,
            "model": "m-Q4_K_M.gguf", "mmproj": "mmproj-F16.gguf",
        });
        let mut job = json!({"requires": ["os:ubuntu"], "spec": cli_spec(model, None)});
        job["spec"]["benchmark"] = json!(benchmark);
        if let Some(thinking) = thinking {
            job["spec"]["model_flags"] = json!({"benchmark_type": flags_for, "model_type": "gguf_vision", "enable_thinking": thinking});
        }
        job
    }

    #[rstest]
    // Either repo the recipe covers, thinking stated off.
    #[case::on_recipe(
        asr_job(
            "asr-nl-voxtral",
            "ggml-org",
            "Voxtral-Mini-3B-2507-GGUF",
            Some(false),
            "asr_eval"
        ),
        None
    )]
    #[case::second_repo(
        asr_job(
            "asr-nl-voxtral",
            "bartowski",
            "mistralai_Voxtral-Mini-3B-2507-GGUF",
            Some(false),
            "asr_eval"
        ),
        None
    )]
    #[case::another_repo(asr_job("asr-nl-voxtral", "ggml-org", "gemma-4-E2B-it-GGUF", Some(false), "asr_eval"), Some((|e| matches!(e, VlmEvalRecipeError::NotCovered { .. })) as RecipeKind))]
    #[case::thinking_on(asr_job("asr-nl-voxtral", "ggml-org", "Voxtral-Mini-3B-2507-GGUF", Some(true), "asr_eval"), Some((|e| matches!(e, VlmEvalRecipeError::CellThinking { benchmark: BenchmarkType::AsrEval, .. })) as RecipeKind))]
    #[case::thinking_unstated(asr_job("asr-nl-voxtral", "ggml-org", "Voxtral-Mini-3B-2507-GGUF", None, "asr_eval"), Some((|e| matches!(e, VlmEvalRecipeError::CellThinking { .. })) as RecipeKind))]
    fn an_asr_eval_cell_runs_its_entrys_recipe(
        #[case] body: Value,
        #[case] expected: Option<RecipeKind>,
    ) -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let catalog = asr_eval_catalog(&pipette_plan_types::OrgId::mint(), &[])?;
        let checked = validate_job(&body, None, &catalog, ModelCheck::none(), Utc::now());
        match expected {
            None => anyhow::ensure!(checked.is_ok(), "{checked:?}"),
            Some(expected) => anyhow::ensure!(
                matches!(&checked, Err(JobRejectionError::OffRecipe { reason, .. }) if expected(reason)),
                "wrong rejection: {checked:?}"
            ),
        }
        Ok(())
    }

    /// A vlm_eval cell's flags are not an asr_eval cell's: the recipe cannot
    /// read a mode from them.
    #[test]
    fn an_asr_eval_cell_states_its_mode_in_its_own_flags() -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let catalog = asr_eval_catalog(&pipette_plan_types::OrgId::mint(), &[])?;
        let body = asr_job(
            "asr-nl-voxtral",
            "ggml-org",
            "Voxtral-Mini-3B-2507-GGUF",
            Some(false),
            "vlm_eval",
        );
        let checked = validate_job(&body, None, &catalog, ModelCheck::none(), Utc::now());
        anyhow::ensure!(
            matches!(
                &checked,
                Err(JobRejectionError::RecipeUnchecked {
                    field: "model_flags",
                    ..
                })
            ),
            "{checked:?}"
        );
        Ok(())
    }

    /// Two of one organization's asr_eval entries covering one model for one
    /// task are ambiguous; another organization's, or another task's, are not.
    #[test]
    fn a_model_two_asr_recipes_cover_is_ambiguous_only_within_its_organization()
    -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let org = pipette_plan_types::OrgId::mint();
        let rival = "family = \"Voxtral rival\"\nmodels = [{ repo = \"GGML-org/voxtral-mini-3b-2507-gguf\" }]";
        let mut catalog = asr_eval_catalog(&org, &[("asr-nl-rival", "asr_fleurs_nl_nl", rival)])?;
        let job = |id: &str| {
            asr_job(
                id,
                "ggml-org",
                "Voxtral-Mini-3B-2507-GGUF",
                Some(false),
                "asr_eval",
            )
        };
        let check = |catalog: &HashMap<BenchmarkId, Benchmark>, id: &str| {
            validate_job(&job(id), None, catalog, ModelCheck::none(), Utc::now())
        };
        // A public rival is ambiguous for the private entry's filer, who can
        // name both.
        let Err(rejection) = check(&catalog, "asr-nl-voxtral") else {
            anyhow::bail!("the job ran under one of two recipes");
        };
        assert_eq!(
            rejection.to_string(),
            "ggml-org/Voxtral-Mini-3B-2507-GGUF is covered by two recipes for asr_fleurs_nl_nl \
             2026.10.1: asr-nl-voxtral and asr-nl-rival"
        );
        // Moved to another organization, it no longer is.
        let id = BenchmarkId::try_new("asr-nl-rival")?;
        let rival = catalog.remove(&id).context("rival")?;
        catalog.insert(id, rival.private_to(pipette_plan_types::OrgId::mint()));
        check(&catalog, "asr-nl-voxtral")?;
        // Another task's entry covering the same model is not a rival.
        check(&catalog, "asr-de-voxtral")?;
        Ok(())
    }

    // ── validation: an asr_eval cell on vLLM ────────────────────────────────

    /// An asr_eval job for `benchmark` running the torch weights of `org/repo`
    /// on vLLM, thinking stated off.
    fn asr_vllm_job(benchmark: &str, org: &str, repo: &str) -> Value {
        let model =
            json!({"type": "torch", "source": "huggingface", "org": org, "repo_name": repo});
        let flags = json!({"benchmark_type": "asr_eval", "runtime_type": "uv_vllm", "model_type": "torch", "parallel": 64});
        json!({"requires": ["os:ubuntu"], "spec": {
            "benchmark": benchmark, "model": model, "runtime": uv_vllm(), "runtime_flags": flags,
            "model_flags": {"benchmark_type": "asr_eval", "model_type": "torch", "enable_thinking": false},
        }})
    }

    /// A covered repo's own weights run an asr_eval on uv vLLM under its
    /// entry's recipe, as a vlm_eval's do; on another runtime the cell does
    /// not exist, and flags tagged for a vlm_eval are not its own.
    #[rstest]
    #[case::on_recipe(
        asr_vllm_job("asr-nl-voxtral", "mistralai", "Voxtral-Mini-3B-2507"),
        None
    )]
    #[case::uncovered(
        asr_vllm_job("asr-nl-gemma", "mistralai", "Voxtral-Mini-3B-2507"),
        Some("does not cover mistralai/Voxtral-Mini-3B-2507")
    )]
    #[case::another_runtime(
        {
            let mut job = asr_vllm_job("asr-nl-voxtral", "mistralai", "Voxtral-Mini-3B-2507");
            job["spec"]["runtime"] = docker_sglang();
            if let Some(spec) = job["spec"].as_object_mut() {
                spec.remove("runtime_flags");
            }
            job
        },
        Some("on uv_vllm alone")
    )]
    #[case::vlm_eval_flags(
        {
            let mut job = asr_vllm_job("asr-nl-voxtral", "mistralai", "Voxtral-Mini-3B-2507");
            job["spec"]["runtime_flags"]["benchmark_type"] = json!("vlm_eval");
            job
        },
        Some("they are for a \"vlm_eval\" cell")
    )]
    fn an_asr_eval_cell_on_vllm_runs_its_entrys_recipe(
        #[case] body: Value,
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let catalog = asr_eval_catalog(&pipette_plan_types::OrgId::mint(), &[])?;
        let checked = validate_job(&body, None, &catalog, ModelCheck::none(), Utc::now());
        match refused {
            None => checked.map(|_| ())?,
            Some(names) => {
                let error = checked.err().map(|e| e.to_string()).unwrap_or_default();
                anyhow::ensure!(error.contains(names), "{error}");
            }
        }
        Ok(())
    }

    /// vLLM runs no LFM audio model, and its beam search has no repetition
    /// penalty for MiniCPM-o's beam recipe: such entries run on llama.cpp
    /// alone, where the same entries are accepted.
    #[rstest]
    #[case::beams(
        "minicpm_o",
        "parameter_num_beams = 3\n",
        "MiniCPM-o",
        "openbmb/MiniCPM-o-4_5",
        "its beam search takes no repetition penalty"
    )]
    #[case::lfm(
        "lfm25_audio",
        "",
        "LFM2.5-Audio",
        "LiquidAI/LFM2.5-Audio-1.5B",
        "vLLM has no implementation of its model"
    )]
    fn an_asr_eval_entry_vllm_cannot_run_is_refused_there(
        #[case] profile: &str,
        #[case] decoding: &str,
        #[case] family: &str,
        #[case] repo: &str,
        #[case] refused: &str,
    ) -> anyhow::Result<()> {
        let (org, name) = repo.split_once('/').context("org/repo")?;
        let toml = format!(
            "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"asr_fleurs_nl_nl\"\n\
             parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 364\n\
             parameter_prompt_profile = \"{profile}\"\nparameter_max_tokens = 500\n{decoding}\
             [dataset]\nformat = \"asr-eval-corpus/1\"\nadapter = \"evals-server@1\"\n\
             digest = \"{}\"\nbytes = 1024\n\
             [dataset.upstream]\nsource = \"evals_server\"\neval_id = \"asr_fleurs_nl_nl\"\n\
             dataset_version = \"2026.10.1\"\nprompt_profile = \"{profile}\"\nidentity = \"{}\"\n\
             [recipe]\nfamily = \"{family}\"\nmodels = [{{ repo = \"{repo}\" }}, {{ repo = \"{repo}-GGUF\" }}]\n\
             enable_thinking = false\nsources = [\"https://huggingface.co/x\"]\n",
            "1".repeat(64),
            "2".repeat(64),
        );
        let id = "asr-nl-x";
        let catalog =
            HashMap::from([(BenchmarkId::try_new(id)?, Benchmark::from_toml(id, &toml)?)]);
        let on_vllm = validate_job(
            &asr_vllm_job(id, org, name),
            None,
            &catalog,
            ModelCheck::none(),
            Utc::now(),
        );
        anyhow::ensure!(
            matches!(&on_vllm, Err(JobRejectionError::AsrEvalRuntime { .. }))
                && on_vllm
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.to_string().contains(refused)),
            "{on_vllm:?}"
        );
        // The same entry on llama.cpp, with the repo's GGUF.
        validate_job(
            &asr_job(id, org, &format!("{name}-GGUF"), Some(false), "asr_eval"),
            None,
            &catalog,
            ModelCheck::none(),
            Utc::now(),
        )?;
        Ok(())
    }

    // ── validation: acceptances ─────────────────────────────────────────────

    #[rstest]
    #[case::requires_only(json!({"requires":["os:macos"],"spec":spec_bench_1()}))]
    #[case::a_compatible_pair(json!({"requires":["os:macos"],"spec":cli_spec(gguf_text(), None)}))]
    // The benchmark is the client's to resolve: `bench-1` has no type, and an
    // eval entry for the pair passes, as do model and benchmark flags.
    #[case::runtime_flags_for_the_pair(json!({"requires":["os:macos"],"spec":cli_spec(gguf_text(), Some(cli_flags("gguf_text")))}))]
    #[case::model_and_benchmark_flags_unread(json!({"requires":["os:macos"],"spec":foreign_model_and_benchmark_flags()}))]
    // A partner's own format, or a client newer than this build.
    #[case::a_spec_this_build_cannot_parse(json!({"requires":["os:macos"],"spec":cli_spec(json!({"type":"partner_format"}), None)}))]
    // Flags for no cell fail the parse, so the spec stays opaque.
    #[case::runtime_flags_this_build_cannot_parse(json!({"requires":["os:macos"],"spec":flags_for_no_cell()}))]
    #[case::clients_only(json!({"clients":["ev1_a"],"spec":spec_bench_1()}))]
    #[case::any_of_same_namespace(json!({"requires":["os:ios"],"any_of":[["os:ios","os:android"]],"spec":spec_bench_1()}))]
    // Free-form (non-reserved) flags may share a prefix.
    #[case::free_form_shared_prefix(json!({"requires":["runtime:llama_cpp","runtime:llama_cpp:b1"],"spec":spec_bench_1()}))]
    #[case::valid_expires_at(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"2999-01-01T00:00:00Z"}))]
    #[case::never_expires(json!({"requires":["os:macos"],"spec":spec_bench_1(),"expires_at":"never"}))]
    // Both spellings of the merge field, plus the explicit null that means
    // "absent" everywhere else in the envelope.
    #[case::merge_opt_out(json!({"requires":["os:macos"],"spec":spec_bench_1(),"merge_duplicates":false}))]
    #[case::merge_opt_in(json!({"requires":["os:macos"],"spec":spec_bench_1(),"merge_duplicates":true}))]
    #[case::merge_null(json!({"requires":["os:macos"],"spec":spec_bench_1(),"merge_duplicates":null}))]
    // The negatives, well-formed and satisfiable.
    #[case::excludes_same_namespace(json!({"requires":["os:ios"],"excludes":["chip:a15","chip:a16"],"spec":spec_bench_1()}))]
    #[case::not_all_of_mixed_namespaces(json!({"requires":["os:ios"],"not_all_of":[["chip:a15","runtime:mlx"]],"spec":spec_bench_1()}))]
    #[case::exclude_clients_beside_roster(json!({"clients":["ev1_a","ev1_b"],"exclude_clients":["ev1_b"],"spec":spec_bench_1()}))]
    // Fleet-dependent, not unmatchable: no client can match this body today, and
    // one that registers tomorrow can. That is a warning, never a rejection.
    #[case::fleet_dependent_exclusion(json!({"requires":["os:ios"],"exclude_clients":["ev1_a"],"spec":spec_bench_1()}))]
    fn validate_job_accepts(#[case] body: Value) -> anyhow::Result<()> {
        check(&body)??;
        Ok(())
    }

    /// An acceptance case is only a check on `validate` if its spec parses.
    #[test]
    fn the_acceptance_specs_parse_only_where_meant() {
        let parses = |spec: Value| ClientRunSpec::deserialize(spec).is_ok();
        assert!(parses(cli_spec(gguf_text(), Some(cli_flags("gguf_text")))));
        assert!(parses(foreign_model_and_benchmark_flags()));
        assert!(!parses(cli_spec(json!({"type": "partner_format"}), None)));
        assert!(!parses(flags_for_no_cell()));
    }

    #[test]
    fn an_unrunnable_spec_says_why() -> anyhow::Result<()> {
        let body = json!({"requires": ["os:macos"], "spec": cli_spec(mlx(), None)});
        let message = check(&body)?.err().map(|err| err.to_string());
        assert_eq!(
            message.as_deref(),
            Some("`spec` cannot run: runtime `llamacpp_cli_stock_tools` cannot load a `mlx` model")
        );
        Ok(())
    }

    // ── warning grouping & message text ─────────────────────────────────────

    /// A minted placement over `body`, for the grouping assertions below.
    fn placed(job_id: &str, body: Value) -> Placement {
        Placement {
            label: format!("{job_id}.json"),
            job_id: JobId::new_unchecked(job_id),
            body,
            expires_at: ExpiresAt::Never,
            horizon: ExpiresAt::Never,
            merged: None,
            digest: String::new(),
            settled: false,
            parent: None,
            hf_model: None,
        }
    }

    /// Unmatched jobs group by requirement set, whatever the flag order; a
    /// difference in any clause, negatives included, splits the group.
    #[rstest]
    #[case::group_by_requirement_set(
        &[
            ("job-a", json!({"requires": ["os:ios", "runtime:mlx"]})),
            // Same requirement *set*, different textual order — joins a's group.
            ("job-b", json!({"requires": ["runtime:mlx", "os:ios"]})),
            ("job-c", json!({"clients": ["ev1_x"]})),
            ("job-d", json!({"requires": ["os:ios"], "any_of": [["device:a", "device:b"]]})),
        ],
        &[
            (
                &["job-a", "job-b"][..],
                "2 jobs requiring os:ios, runtime:mlx match no working client",
            ),
            // Singular agrees in number ("1 job … matches", not "1 job … match").
            (&["job-c"][..], "1 job pinned to ev1_x matches no working client"),
            (
                &["job-d"][..],
                "1 job requiring os:ios with any-of [device:a, device:b] \
                 matches no working client",
            ),
        ]
    )]
    #[case::separate_by_exclusions(
        &[
            ("job-a", json!({"requires": ["os:ios"]})),
            ("job-b", json!({"requires": ["os:ios"], "excludes": ["chip:a15"]})),
            ("job-c", json!({"requires": ["os:ios"], "not_all_of": [["chip:a15", "runtime:mlx"]]})),
            ("job-d", json!({"requires": ["os:ios"], "exclude_clients": ["ev1_x"]})),
        ],
        &[
            (&["job-a"][..], "1 job requiring os:ios matches no working client"),
            (
                &["job-b"][..],
                "1 job requiring os:ios excluding flags chip:a15 \
                 matches no working client",
            ),
            (
                &["job-c"][..],
                "1 job requiring os:ios with not-all-of [chip:a15, runtime:mlx] \
                 matches no working client",
            ),
            (
                &["job-d"][..],
                "1 job requiring os:ios excluding clients ev1_x \
                 matches no working client",
            ),
        ]
    )]
    fn warnings_group_unmatched_jobs(
        #[case] jobs: &[(&str, Value)],
        #[case] expected: &[(&[&str], &str)],
    ) -> anyhow::Result<()> {
        let placements: Vec<Placement> = jobs
            .iter()
            .map(|(job_id, body)| placed(job_id, body.clone()))
            .collect();
        let warnings = group_warnings(&placements.iter().collect::<Vec<_>>());

        let actual: Vec<(Vec<&str>, &str)> = warnings
            .iter()
            .map(|w| {
                (
                    w.job_ids.iter().map(JobId::as_str).collect(),
                    w.message.as_str(),
                )
            })
            .collect();
        let expected: Vec<(Vec<&str>, &str)> = expected
            .iter()
            .map(|(ids, message)| (ids.to_vec(), *message))
            .collect();
        assert_eq!(actual, expected);
        Ok(())
    }

    // ── report serialization ────────────────────────────────────────────────

    #[test]
    fn report_serializes_job_ids_as_ordered_object_and_omits_absent_name() -> anyhow::Result<()> {
        let report = IngestReport {
            plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
            note: None,
            change: None,
            job_count: 2,
            merged_job_count: 0,
            // Labels chosen so input order and lexical order *disagree*
            // (`"10" < "2"` as strings): a regression to a key-sorting map would
            // emit `job-b` first and fail the assertion below.
            job_ids: vec![
                ("2".to_string(), JobId::new_unchecked("job-a")),
                ("10".to_string(), JobId::new_unchecked("job-b")),
            ],
            merges: Vec::new(),
            warnings: Vec::new(),
            plan_warnings: Vec::new(),
        };
        let text = serde_json::to_string(&report)?;
        assert!(!text.contains("note"), "None note is omitted");
        assert!(
            !text.contains("plan_warnings"),
            "empty plan_warnings are omitted"
        );
        assert!(
            text.contains(r#""job_ids":{"2":"job-a","10":"job-b"}"#),
            "cardinal keys preserve input order, not lexical order: {text}"
        );
        Ok(())
    }

    // ── end-to-end on the local-fs backend ──────────────────────────────────

    /// Register `client` as a lab device: the record, and the `perform_work`
    /// grant the fleet match counts. A test about the missing grant writes the
    /// record alone.
    async fn enlist(stores: &Stores, client: &Client) -> anyhow::Result<()> {
        stores.clients.put_client(client).await?;
        grant_perform_work(stores, client).await
    }

    async fn grant_perform_work(stores: &Stores, client: &Client) -> anyhow::Result<()> {
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

    fn stores_in(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = Config {
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..Config::default()
        };
        build_local_fs_stores(&config)
    }

    fn client(id: &str, os: &str, status: ClientStatus) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("d")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status,
            registered_at: Utc::now(),
            device_profile: DeviceProfile {
                device_os_name: Some(NonEmptyTrimmedString::try_new(os)?),
                ..Default::default()
            },
            capabilities: Default::default(),
        })
    }

    /// Ingest with no submitter, as the CLI does. The default for every
    /// test that is not about the submitter itself.
    async fn ingest(
        stores: &Stores,
        catalog: &HashMap<BenchmarkId, Benchmark>,
        note: Option<NonEmptyTrimmedString>,
        jobs: Vec<(String, Value)>,
    ) -> Result<IngestReport, IngestError> {
        ingest_as(stores, catalog, note, None, jobs).await
    }

    /// Ingest as a named submitter, the shape the API's plan filing uses, with
    /// no model parameter table.
    async fn ingest_as(
        stores: &Stores,
        catalog: &HashMap<BenchmarkId, Benchmark>,
        note: Option<NonEmptyTrimmedString>,
        submitted_by: Option<UserId>,
        jobs: Vec<(String, Value)>,
    ) -> Result<IngestReport, IngestError> {
        ingest_with_models(
            stores,
            catalog,
            ModelCheck::none(),
            note,
            submitted_by,
            jobs,
        )
        .await
    }

    /// [`ingest_as`] with the model coverage check spelled out.
    async fn ingest_with_models(
        stores: &Stores,
        catalog: &HashMap<BenchmarkId, Benchmark>,
        models: ModelCheck<'_>,
        note: Option<NonEmptyTrimmedString>,
        submitted_by: Option<UserId>,
        jobs: Vec<(String, Value)>,
    ) -> Result<IngestReport, IngestError> {
        ingest_limited(stores, catalog, models, note, submitted_by, jobs, MAX_JOBS).await
    }

    /// [`ingest_with_models`] with the job ceiling spelled out.
    async fn ingest_limited(
        stores: &Stores,
        catalog: &HashMap<BenchmarkId, Benchmark>,
        models: ModelCheck<'_>,
        note: Option<NonEmptyTrimmedString>,
        submitted_by: Option<UserId>,
        jobs: Vec<(String, Value)>,
        max_jobs: std::num::NonZeroUsize,
    ) -> Result<IngestReport, IngestError> {
        ingest_jobs(
            &IngestContext::from_stores(stores, catalog, models, max_jobs, CREATING_MAX_AGE),
            PlanSubmission {
                note,
                change: None,
                submitted_by,
                membership: CampaignMembership::for_test(
                    UserId::try_new("user_test").map_err(|e| IngestError::Other(e.into()))?,
                ),
                jobs,
                source: None,
                plan_warnings: Vec::new(),
            },
        )
        .await
    }

    /// A plan filed from a document stores the document beside the manifest,
    /// and the manifest records its digest. The pending marker is gone once the
    /// manifest exists. A plan filed as job bodies writes neither.
    #[rstest]
    #[case::from_a_document(Some("benchmarks = [\"bench-1\"]\n# kept as written\n"))]
    #[case::from_job_bodies(None)]
    #[tokio::test]
    async fn ingest_stores_the_plan_source_before_the_manifest(
        #[case] document: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let source = document
            .map(|text| PlanSource::new(text.to_string()))
            .transpose()?;
        let catalog = catalog()?;

        let report = ingest_jobs(
            &IngestContext::from_stores(
                &stores,
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
                jobs: vec![(
                    "job.json".to_string(),
                    json!({"requires":["os:macos"],"spec":spec_bench_1()}),
                )],
                source: source.clone(),
                plan_warnings: Vec::new(),
            },
        )
        .await?;

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("the plan was just ingested")?;
        assert_eq!(
            manifest.source_sha256.as_ref(),
            source.as_ref().map(PlanSource::sha256)
        );
        let stored = stores.plans.get_plan_source(&report.plan_id).await?;
        assert_eq!(
            stored.as_ref().map(PlanSource::text),
            document,
            "the stored source is the submitted text, byte for byte"
        );
        assert!(
            stores.plans.list_pending_source_markers().await?.is_empty(),
            "no pending marker outlives a finished ingest"
        );
        Ok(())
    }

    /// Every job of a new plan gets a job source equal to its queued body, and
    /// a link to the plan. This holds for a plan from a document and for a
    /// plan from job bodies.
    #[rstest]
    #[case::from_a_document(Some("benchmarks = [\"bench-1\"]\n"))]
    #[case::from_job_bodies(None)]
    #[tokio::test]
    async fn ingest_stores_each_jobs_source_and_link(
        #[case] document: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let catalog = catalog()?;

        let report = ingest_jobs(
            &IngestContext::from_stores(
                &stores,
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
                jobs: vec![
                    ("a.json".to_string(), cell_body("a", json!({}))),
                    ("b.json".to_string(), cell_body("b", json!({}))),
                ],
                source: document
                    .map(|text| PlanSource::new(text.to_string()))
                    .transpose()?,
                plan_warnings: Vec::new(),
            },
        )
        .await?;

        let job_ids: Vec<JobId> = report.job_ids.iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(job_ids.len(), 2);
        assert_each_job_has_its_source_and_link(
            stores.plans.as_ref(),
            stores.todo.as_ref(),
            &report.plan_id,
            &job_ids,
        )
        .await?;
        assert_eq!(local_fs_job_record_count(dir.path()), 4);
        Ok(())
    }

    /// One write of the source path in [`ingest_jobs`], in the order the ingest
    /// makes them.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SourceStep {
        Marker,
        /// The plan document.
        Source,
        Manifest,
        MarkerDelete,
        JobSource,
        Link,
        /// The best-effort index of a minted job's cell.
        JobCell,
    }

    /// A [`PlanStore`] that fails one [`SourceStep`] and delegates everything
    /// else, recording the plan id of the marker write.
    struct FailsOneSourceStep {
        inner: std::sync::Arc<dyn PlanStore>,
        fail: SourceStep,
        plan_id: std::sync::Mutex<Option<PlanId>>,
        /// A source that the store holds under the minted plan id before the
        /// ingest puts its own.
        squatter: Option<PlanSource>,
        /// Each cell the ingest indexed, with its job.
        cells: std::sync::Mutex<Vec<(JobId, String)>>,
    }

    impl FailsOneSourceStep {
        fn new(inner: std::sync::Arc<dyn PlanStore>, fail: SourceStep) -> Self {
            Self {
                inner,
                fail,
                plan_id: std::sync::Mutex::new(None),
                squatter: None,
                cells: Default::default(),
            }
        }

        fn fails_at(&self, step: SourceStep) -> anyhow::Result<()> {
            if self.fail == step {
                anyhow::bail!("simulated {step:?} failure")
            }
            Ok(())
        }

        fn plan_id(&self) -> anyhow::Result<PlanId> {
            self.plan_id
                .lock()
                .map_err(|e| anyhow::anyhow!("poisoned: {e}"))?
                .clone()
                .context("the ingest wrote no pending marker")
        }
    }

    #[async_trait::async_trait]
    impl PlanStore for FailsOneSourceStep {
        async fn put_job_cell(
            &self,
            job_id: &JobId,
            cell_digest: &str,
            scope: &CellScope,
        ) -> anyhow::Result<()> {
            self.cells
                .lock()
                .map_err(|e| anyhow::anyhow!("poisoned: {e}"))?
                .push((job_id.clone(), cell_digest.to_string()));
            self.fails_at(SourceStep::JobCell)?;
            self.inner.put_job_cell(job_id, cell_digest, scope).await
        }

        async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            *self
                .plan_id
                .lock()
                .map_err(|e| anyhow::anyhow!("poisoned: {e}"))? = Some(plan_id.clone());
            self.fails_at(SourceStep::Marker)?;
            self.inner.write_pending_source_marker(plan_id).await
        }
        async fn put_plan_source(
            &self,
            plan_id: &PlanId,
            source: &PlanSource,
        ) -> anyhow::Result<CreateOutcome> {
            self.fails_at(SourceStep::Source)?;
            if let Some(squatter) = &self.squatter {
                self.inner.put_plan_source(plan_id, squatter).await?;
            }
            self.inner.put_plan_source(plan_id, source).await
        }
        async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
            if manifest.status == PlanStatus::Creating {
                self.fails_at(SourceStep::Manifest)?;
            }
            self.inner.put_plan(manifest).await
        }
        async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.fails_at(SourceStep::MarkerDelete)?;
            self.inner.delete_pending_source_marker(plan_id).await
        }
        async fn put_job_source(
            &self,
            job_id: &JobId,
            body: &serde_json::Value,
        ) -> anyhow::Result<CreateOutcome> {
            self.fails_at(SourceStep::JobSource)?;
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
            self.fails_at(SourceStep::Link)?;
            self.inner.put_job_plan_link(job_id, plan_id).await
        }
        async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
            self.inner.list_job_plan_links(job_id).await
        }
        async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
            self.inner.get_plan(plan_id).await
        }
        async fn list_matching_plans(
            &self,
            filter: &pipette_store::PlanFilter,
            after: Option<&PlanId>,
            limit: NonZeroUsize,
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
        async fn list_all_job_plan_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_store::JobPlanMarker>> {
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
        async fn get_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanSource>> {
            self.inner.get_plan_source(plan_id).await
        }
        async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()> {
            self.inner.delete_plan_source(plan_id).await
        }
        async fn list_pending_source_markers(
            &self,
        ) -> anyhow::Result<Vec<pipette_store::PendingSourceMarker>> {
            self.inner.list_pending_source_markers().await
        }
    }

    /// The source path writes the marker, the source, and the manifest in that
    /// order. An ingest cut at any step leaves no manifest that names a missing
    /// source, and a marker names every source that has no manifest. A failed
    /// marker delete does not fail the ingest.
    #[rstest]
    #[case::marker(SourceStep::Marker, false, false, false)]
    #[case::source(SourceStep::Source, true, false, false)]
    #[case::manifest(SourceStep::Manifest, true, true, false)]
    #[case::marker_delete(SourceStep::MarkerDelete, true, true, true)]
    #[tokio::test]
    async fn a_cut_ingest_leaves_no_manifest_without_its_source(
        #[case] fail: SourceStep,
        #[case] marker_survives: bool,
        #[case] source_survives: bool,
        #[case] ingested: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let plans = FailsOneSourceStep::new(stores.plans.clone(), fail);
        let catalog = catalog()?;

        let result = ingest_jobs(
            &IngestContext {
                plans: &plans,
                ..IngestContext::from_stores(
                    &stores,
                    &catalog,
                    ModelCheck::none(),
                    MAX_JOBS,
                    CREATING_MAX_AGE,
                )
            },
            PlanSubmission {
                note: None,
                change: None,
                submitted_by: None,
                membership: CampaignMembership::for_test(UserId::try_new("user_test")?),
                jobs: vec![(
                    "job.json".to_string(),
                    json!({"requires":["os:macos"],"spec":spec_bench_1()}),
                )],
                source: Some(PlanSource::new("benchmarks = [\"bench-1\"]\n".to_string())?),
                plan_warnings: Vec::new(),
            },
        )
        .await;

        assert_eq!(result.is_ok(), ingested, "{fail:?}: {result:?}");
        let plan_id = plans.plan_id()?;
        let markers: Vec<PlanId> = stores
            .plans
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|marker| marker.plan_id)
            .collect();
        assert_eq!(
            markers,
            marker_survives
                .then(|| plan_id.clone())
                .into_iter()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            stores.plans.get_plan_source(&plan_id).await?.is_some(),
            source_survives
        );
        assert_eq!(stores.plans.get_plan(&plan_id).await?.is_some(), ingested);
        Ok(())
    }

    /// Each job writes its source, then its link, then stages. An ingest cut at
    /// the source or the link queues nothing, and leaves no link without a job
    /// source.
    #[rstest]
    #[case::job_source(SourceStep::JobSource, 0)]
    #[case::link(SourceStep::Link, 1)]
    #[tokio::test]
    async fn a_cut_job_write_queues_nothing(
        #[case] fail: SourceStep,
        #[case] job_records: usize,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let plans = FailsOneSourceStep::new(stores.plans.clone(), fail);
        let catalog = catalog()?;

        let result = ingest_jobs(
            &IngestContext {
                plans: &plans,
                ..IngestContext::from_stores(
                    &stores,
                    &catalog,
                    ModelCheck::none(),
                    MAX_JOBS,
                    CREATING_MAX_AGE,
                )
            },
            PlanSubmission {
                note: None,
                change: None,
                submitted_by: None,
                membership: CampaignMembership::for_test(UserId::try_new("user_test")?),
                jobs: vec![(
                    "job.json".to_string(),
                    json!({"requires":["os:macos"],"spec":spec_bench_1()}),
                )],
                source: None,
                plan_warnings: Vec::new(),
            },
        )
        .await;

        assert!(result.is_err(), "{fail:?}: {result:?}");
        assert!(stores.todo.list_avail().await?.is_empty(), "{fail:?}");
        assert_eq!(
            local_fs_job_record_count(dir.path()),
            job_records,
            "{fail:?}"
        );
        Ok(())
    }

    /// Ingest `jobs` as job bodies through `plans`, with the rest of `stores`.
    async fn ingest_through(
        plans: &FailsOneSourceStep,
        stores: &Stores,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        let catalog = catalog()?;
        Ok(ingest_jobs(
            &IngestContext {
                plans,
                ..IngestContext::from_stores(
                    stores,
                    &catalog,
                    ModelCheck::none(),
                    MAX_JOBS,
                    CREATING_MAX_AGE,
                )
            },
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
        .await?)
    }

    /// Each job a plan mints indexes the cell digest of its source, once. A
    /// sibling that shares the job, and a later plan that joins it, index none.
    #[tokio::test]
    async fn a_minted_job_indexes_its_cell_and_a_join_does_not() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        // A plan filed as job bodies writes no marker, so no step fails.
        let plans = FailsOneSourceStep::new(stores.plans.clone(), SourceStep::MarkerDelete);
        let twins = vec![
            ("a.json".to_string(), cell_body("a", json!({}))),
            ("a-again.json".to_string(), cell_body("a", json!({}))),
            ("b.json".to_string(), cell_body("b", json!({}))),
        ];

        let first = ingest_through(&plans, &stores, twins).await?;
        let joiner = vec![("a.json".to_string(), cell_body("a", json!({})))];
        let second = ingest_through(&plans, &stores, joiner).await?;

        assert_eq!(second.merges.len(), 1, "the second plan joins the job");
        let mut expected: Vec<(JobId, String)> =
            futures::stream::iter([&first.job_ids[0].1, &first.job_ids[2].1])
                .then(|job_id| async {
                    let source = stores
                        .plans
                        .get_job_source(job_id)
                        .await?
                        .context("the source of a minted job")?;
                    anyhow::Ok((job_id.clone(), crate::content_key::cell_digest(&source)))
                })
                .try_collect()
                .await?;
        expected.sort();
        let mut cells = plans
            .cells
            .lock()
            .map_err(|e| anyhow::anyhow!("poisoned: {e}"))?
            .clone();
        cells.sort();
        assert_eq!(cells, expected);
        Ok(())
    }

    /// The cell index is best effort: a failed write still queues the job.
    #[tokio::test]
    async fn a_failed_cell_write_still_queues_the_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let plans = FailsOneSourceStep::new(stores.plans.clone(), SourceStep::JobCell);

        let report = ingest_through(
            &plans,
            &stores,
            vec![("a.json".to_string(), cell_body("a", json!({})))],
        )
        .await?;

        assert_eq!(report.job_count, 1);
        assert_eq!(stores.todo.list_avail().await?.len(), 1);
        Ok(())
    }

    /// A plan source that already exists under the minted plan id fails the
    /// ingest before the manifest. The store keeps the first source.
    #[tokio::test]
    async fn an_existing_plan_source_fails_the_ingest() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let squatter = PlanSource::new("benchmarks = [\"squatter\"]\n".to_string())?;
        let plans = FailsOneSourceStep {
            squatter: Some(squatter.clone()),
            // No step fails: the squatter alone stops the ingest.
            ..FailsOneSourceStep::new(stores.plans.clone(), SourceStep::MarkerDelete)
        };
        let catalog = catalog()?;

        let result = ingest_jobs(
            &IngestContext {
                plans: &plans,
                ..IngestContext::from_stores(
                    &stores,
                    &catalog,
                    ModelCheck::none(),
                    MAX_JOBS,
                    CREATING_MAX_AGE,
                )
            },
            PlanSubmission {
                note: None,
                change: None,
                submitted_by: None,
                membership: CampaignMembership::for_test(UserId::try_new("user_test")?),
                jobs: vec![(
                    "job.json".to_string(),
                    json!({"requires":["os:macos"],"spec":spec_bench_1()}),
                )],
                source: Some(PlanSource::new("benchmarks = [\"bench-1\"]\n".to_string())?),
                plan_warnings: Vec::new(),
            },
        )
        .await;

        let Err(IngestError::Other(e)) = result else {
            anyhow::bail!("expected a store failure, got {result:?}");
        };
        assert!(e.downcast_ref::<PlanSourceExistsError>().is_some(), "{e:#}");
        let plan_id = plans.plan_id()?;
        assert_eq!(
            stores
                .plans
                .get_plan_source(&plan_id)
                .await?
                .map(|s| s.sha256().clone()),
            Some(squatter.sha256().clone())
        );
        assert!(stores.plans.get_plan(&plan_id).await?.is_none());
        Ok(())
    }

    /// Ingestion writes the submitter the transport hands it onto the manifest,
    /// and nothing when it hands none.
    #[rstest]
    #[case::user_account(Some(USER_ADA), Some(USER_ADA))]
    #[case::by_operator(None, None)]
    #[case::by_a_client(None, None)]
    #[tokio::test]
    async fn ingest_records_the_submitter_on_the_manifest(
        #[case] submitted_by: Option<&str>,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;

        let jobs = vec![(
            "job.json".to_string(),
            json!({"requires":["os:macos"],"spec":spec_bench_1()}),
        )];
        let submitter = submitted_by.map(UserId::try_new).transpose()?;
        let report = ingest_as(&stores, &catalog()?, None, submitter, jobs).await?;

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("the plan was just ingested")?;
        assert_eq!(manifest.submitted_by.as_ref().map(UserId::as_str), expected,);

        // A plan with no submitter omits the key, so an operator-ingested
        // manifest is byte-identical to one written before the field existed.
        let serialized = serde_json::to_value(&manifest)?;
        assert_eq!(
            serialized.get("submitted_by").is_some(),
            expected.is_some(),
            "the key is present exactly when the transport named a submitter"
        );
        Ok(())
    }

    #[tokio::test]
    async fn ingest_stages_jobs_active_with_grouped_warning() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;

        let jobs = vec![
            (
                "match.json".to_string(),
                json!({"requires":["os:macos"],"spec":spec_bench_1()}),
            ),
            (
                "starve.json".to_string(),
                json!({"requires":["os:ios"],"spec":spec_bench_1()}),
            ),
        ];
        let report = ingest(
            &stores,
            &catalog()?,
            Some(NonEmptyTrimmedString::try_new("smoke")?),
            jobs,
        )
        .await?;

        assert_eq!(report.job_count, 2);
        assert!(report.plan_id.as_str().starts_with("plan-"));
        assert_eq!(report.job_ids[0].0, "match.json");
        // Only the os:ios job is starved, and the warning names exactly its id.
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.warnings[0].job_ids,
            vec![report.job_ids[1].1.clone()]
        );

        // Both jobs are in avail/, each stamped with its minted job_id.
        let todo = &stores.todo;
        futures::stream::iter(report.job_ids.iter().map(anyhow::Ok))
            .try_for_each(|(_, job_id)| async move {
                let body = todo
                    .get_avail_by_job(job_id)
                    .await?
                    .context("job promoted to avail/")?;
                assert_eq!(
                    body.get("job_id").and_then(Value::as_str),
                    Some(job_id.as_str())
                );
                anyhow::Ok(())
            })
            .await?;

        // Manifest finalized to active, carrying the full id list and the frozen
        // warning.
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest written")?;
        assert_eq!(manifest.status, PlanStatus::Active);
        assert_eq!(manifest.job_ids.len(), 2);
        assert_eq!(manifest.warnings.len(), 1);
        assert_eq!(
            report.note.as_ref().map(NonEmptyTrimmedString::as_str),
            Some("smoke")
        );
        Ok(())
    }

    /// A job whose `expires_at` is already past refuses the whole plan, and
    /// nothing is written.
    #[tokio::test]
    async fn ingest_rejects_a_past_expires_at() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let live = json!({"requires":["os:macos"],"spec":spec_bench_1()});
        let mut lapsed = live.clone();
        lapsed["expires_at"] = json!("2020-01-01T00:00:00Z");
        let jobs = vec![
            ("live.json".to_string(), live),
            ("lapsed.json".to_string(), lapsed),
        ];
        let Err(IngestError::Rejected(RejectionError::Job { index, label, kind })) =
            ingest(&stores, &catalog()?, None, jobs).await
        else {
            anyhow::bail!("expected the plan to be rejected");
        };
        assert_eq!((index, label.as_str()), (1, "lapsed.json"));
        assert!(
            matches!(kind, JobRejectionError::PastExpiresAt { .. }),
            "{kind:?}"
        );
        assert!(
            drain_plans(stores.plans.as_ref(), None)
                .await?
                .items
                .is_empty()
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        assert_eq!(local_fs_job_record_count(dir.path()), 0);
        Ok(())
    }

    /// An ingest with no HuggingFace import refuses a model that carries a
    /// token, names the cause, and writes nothing.
    #[tokio::test]
    async fn an_ingest_without_an_import_refuses_a_token() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let mut gated = json!({"requires":["os:macos"],"spec":spec_bench_1()});
        gated["spec"]["model"] = json!({
            "type": "gguf_text", "source": "huggingface", "org": "LiquidAI",
            "repo_name": "LFM2-700M-GGUF", "path": "m.gguf", "auth_token": "hf_SECRET",
        });
        let jobs = vec![("gated.json".to_string(), gated)];
        let Err(IngestError::Rejected(RejectionError::Job { index, kind, .. })) =
            ingest(&stores, &catalog()?, None, jobs).await
        else {
            anyhow::bail!("expected the plan to be rejected");
        };
        assert_eq!(index, 0);
        assert!(
            matches!(kind, JobRejectionError::ImportUnavailable),
            "{kind:?}"
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        Ok(())
    }

    /// A dry run refuses a job set as [`prepare_jobs`] does: the same job, for
    /// the same cause. The import step refuses each set before it calls the
    /// Hub, so neither run has an import client.
    #[rstest]
    #[case::runtime_token(
        json!({"auth_token": "hf_SECRET"}),
        None,
        "AuthToken { field: \"/spec/runtime/auth_token\" }"
    )]
    #[case::no_import(json!(null), Some("hf_SECRET"), "ImportUnavailable")]
    #[tokio::test]
    async fn a_dry_run_refuses_as_preparing_does(
        #[case] runtime_token: Value,
        #[case] model_token: Option<&str>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let catalog = catalog()?;
        let ctx = IngestContext::from_stores(
            &stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );
        let owner = VaultOwner::User {
            user_id: UserId::try_new("user_test")?,
        };
        let mut gated = json!({"requires":["os:macos"],"spec":spec_bench_1()});
        gated["spec"]["model"] = json!({
            "type": "gguf_text", "source": "huggingface", "org": "LiquidAI",
            "repo_name": "LFM2-700M-GGUF", "path": "m.gguf", "auth_token": model_token,
        });
        if let Some(runtime) = gated["spec"]["runtime"].as_object_mut() {
            runtime.extend(runtime_token.as_object().cloned().unwrap_or_default());
        }
        let jobs = vec![("gated.json".to_string(), gated)];

        let refusal = |result: Result<(), IngestError>| match result {
            Err(IngestError::Rejected(RejectionError::Job { index, kind, .. })) => {
                Ok((index, format!("{kind:?}")))
            }
            other => Err(anyhow::anyhow!("expected a job rejection, got {other:?}")),
        };
        let dry_run = refusal(
            dry_run_jobs(&ctx, &owner, jobs.clone(), Utc::now())
                .await
                .map(|_| ()),
        )?;
        let prepared = refusal(prepare_jobs(&ctx, &owner, jobs).await.map(|_| ()))?;

        assert_eq!(dry_run, (0, expected.to_string()));
        assert_eq!(dry_run, prepared);
        Ok(())
    }

    /// Staging refuses an `expires_at` that passed after [`prepare_jobs`]
    /// accepted the set, for example during a long import, and writes nothing.
    #[tokio::test]
    async fn staging_rejects_an_expires_at_that_passed_after_preparation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let catalog = catalog()?;
        let live = json!({"requires":["os:macos"],"spec":spec_bench_1()});
        let mut lapsed = live.clone();
        lapsed["expires_at"] = json!("2020-01-01T00:00:00Z");
        let jobs = [("live.json", live), ("lapsed.json", lapsed)]
            .into_iter()
            .map(|(label, body)| GatedJob {
                label: label.to_string(),
                body,
                hf_model: None,
            })
            .collect();
        let ctx = IngestContext::from_stores(
            &stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );
        let Err(IngestError::Rejected(RejectionError::Job { index, label, kind })) =
            ingest_prepared(
                &ctx,
                PlanSubmission {
                    note: None,
                    change: None,
                    submitted_by: None,
                    membership: CampaignMembership::for_test(UserId::try_new("user_test")?),
                    jobs: PreparedJobs {
                        jobs,
                        rewrites: HashMap::new(),
                    },
                    source: None,
                    plan_warnings: Vec::new(),
                },
            )
            .await
        else {
            anyhow::bail!("expected the plan to be rejected");
        };
        assert_eq!((index, label.as_str()), (1, "lapsed.json"));
        assert!(
            matches!(kind, JobRejectionError::PastExpiresAt { .. }),
            "{kind:?}"
        );
        assert!(
            drain_plans(stores.plans.as_ref(), None)
                .await?
                .items
                .is_empty()
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        assert_eq!(local_fs_job_record_count(dir.path()), 0);
        Ok(())
    }

    /// The expiry resolved at ingestion survives a claim and a `recycle_lease`:
    /// the job returns under the identical `avail/` key.
    #[rstest]
    // No expires_at in the handoff: the 30-day default must survive the round trip.
    #[case::default_expiry(None)]
    #[case::explicit_expiry(Some("2999-01-01T00:00:00Z"))]
    #[case::never_expiry(Some("never"))]
    #[tokio::test]
    async fn ingest_stamps_expiry_so_lease_recycle_preserves_it(
        #[case] expires_at: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let mut body = json!({"requires":["os:macos"],"spec":spec_bench_1()});
        if let Some(ts) = expires_at {
            body["expires_at"] = json!(ts);
        }
        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("j.json".to_string(), body)],
        )
        .await?;
        let job_id = report.job_ids[0].1.clone();

        // The staged expiry, read off the avail/ key.
        let staged_key = stores
            .todo
            .list_avail()
            .await?
            .pop()
            .context("job in avail/")?;
        let (_, staged_expiry) = pipette_core::todo_filename::parse_avail_filename(&staged_key)?;
        match expires_at {
            // The default is `now + 30 days`, which this test cannot name
            // exactly. Its observable property is that it is bounded.
            None => assert_ne!(
                staged_expiry,
                ExpiresAt::Never,
                "a job with no explicit expiry takes the bounded default"
            ),
            Some("never") => assert_eq!(
                staged_expiry,
                ExpiresAt::Never,
                "an explicit `never` stages an unbounded job"
            ),
            Some(ts) => assert_eq!(
                staged_expiry,
                ExpiresAt::At(ts.parse()?),
                "an explicit expiry is honored"
            ),
        }

        // Claim it, then recycle the lease as queue-maintenance would.
        let client_id = ClientId::try_new("ev1_a")?;
        let lease_expiry = Utc::now();
        assert!(matches!(
            stores
                .todo
                .claim_job(&job_id, staged_expiry, &client_id, lease_expiry)
                .await?,
            pipette_store::ClaimResult::Claimed(_)
        ));
        assert_eq!(
            stores
                .todo
                .recycle_lease(&job_id, staged_expiry, &client_id, lease_expiry)
                .await?,
            pipette_store::RecycleResult::Recycled
        );

        // Back in avail/ under the identical key: the round trip preserves the
        // expiry exactly.
        let recycled_key = stores
            .todo
            .list_avail()
            .await?
            .pop()
            .context("job recycled to avail/")?;
        assert_eq!(recycled_key, staged_key);
        Ok(())
    }

    /// Ingestion freezes the min/max of the jobs' resolved expiries on the
    /// manifest. `first_in_days: None` gives both the shared default, so the
    /// range collapses to one value.
    #[rstest]
    #[case::uniform_default(None, None, false)]
    #[case::explicit_and_default(Some(1), None, false)]
    #[case::including_never(Some(1), Some("never"), true)]
    #[tokio::test]
    async fn ingest_freezes_job_expiry_range(
        #[case] first_in_days: Option<i64>,
        #[case] second_expires_at: Option<&str>,
        #[case] latest_is_never: bool,
    ) -> anyhow::Result<()> {
        let first_expires_at = first_in_days.map(|days| {
            (Utc::now() + Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });
        let first_expires_at = first_expires_at.as_deref();
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        // Distinct cells: `expires_at` is outside the content key, so inputs
        // that differ only there would share a job.
        let body = |cell: &str, expires_at: Option<&str>| {
            let mut body = json!({"requires":["os:macos"],"spec":spec_bench_1()});
            body["spec"]["model"] = json!({"name": cell});
            if let Some(at) = expires_at {
                body["expires_at"] = json!(at);
            }
            body
        };
        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                ("one.json".to_string(), body("one", first_expires_at)),
                ("two.json".to_string(), body("two", second_expires_at)),
            ],
        )
        .await?;
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest missing after ingest")?;
        let expiry = manifest.job_expiry.context("job_expiry not frozen")?;
        match first_expires_at {
            None => {
                assert_eq!(expiry.earliest, expiry.latest);
                assert_ne!(expiry.earliest, ExpiresAt::Never);
            }
            Some(at) => {
                assert_eq!(expiry.earliest, ExpiresAt::At(at.parse()?));
                assert!(expiry.earliest < expiry.latest);
            }
        }
        if latest_is_never {
            assert_eq!(expiry.latest, ExpiresAt::Never);
        } else {
            assert_ne!(expiry.latest, ExpiresAt::Never);
        }
        Ok(())
    }

    /// Whatever keeps the fleet from running every job, the plan finalizes as
    /// `pending_clients` with a warning.
    #[rstest]
    #[case::capability_mismatch(ClientStatus::Approved, true, "macOS", "os:ios")]
    #[case::client_not_approved(ClientStatus::Pending, true, "macOS", "os:macos")]
    // The state a machine registers in: it matches the clause and is refused at
    // the claim, so the plan waits for the grant.
    #[case::client_holds_no_perform_work(ClientStatus::Approved, false, "macOS", "os:macos")]
    #[tokio::test]
    async fn ingest_pending_clients_when_nothing_matches(
        #[case] status: ClientStatus,
        #[case] granted: bool,
        #[case] client_os: &str,
        #[case] requires: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let device = client("ev1_a", client_os, status)?;
        stores.clients.put_client(&device).await?;
        if granted {
            grant_perform_work(&stores, &device).await?;
        }

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "only.json".to_string(),
                json!({"requires":[requires],"spec":spec_bench_1()}),
            )],
        )
        .await?;

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.status, PlanStatus::PendingClients);
        assert_eq!(report.warnings.len(), 1);
        Ok(())
    }

    /// The fleet match reads a worker's tags as `tag:` flags, in every clause.
    /// A plan a tagged worker matches is `active`; one no worker matches waits
    /// as `pending_clients` under a warning.
    #[rstest]
    #[case::tagged(&["lab"], json!({"requires": ["tag:lab"]}), PlanStatus::Active)]
    #[case::untagged(&[], json!({"requires": ["tag:lab"]}), PlanStatus::PendingClients)]
    // Two tags in one clause are no contradiction: a client carries many.
    #[case::two_tags(&["lab", "h100"], json!({"requires": ["tag:lab", "tag:h100"]}), PlanStatus::Active)]
    #[case::one_of_two_tags(&["lab"], json!({"requires": ["tag:lab", "tag:h100"]}), PlanStatus::PendingClients)]
    #[case::any_of_tags(&["h100"], json!({"requires": ["os:macos"], "any_of": [["tag:lab", "tag:h100"]]}), PlanStatus::Active)]
    #[case::excluded_tag(&["lab", "busy"], json!({"requires": ["tag:lab"], "excludes": ["tag:busy"]}), PlanStatus::PendingClients)]
    #[case::not_all_of_tags(&["lab", "h100"], json!({"requires": ["tag:lab"], "not_all_of": [["tag:lab", "tag:h100"]]}), PlanStatus::PendingClients)]
    #[tokio::test]
    async fn ingest_matches_a_job_against_client_tags(
        #[case] tags: &[&str],
        #[case] clauses: Value,
        #[case] status: PlanStatus,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let device = client("ev1_a", "macOS", ClientStatus::Approved)?;
        stores.clients.put_client(&device).await?;
        grant_perform_work(&stores, &device).await?;
        let tags = tags
            .iter()
            .map(|tag| pipette_plan_types::Tag::try_new(*tag))
            .collect::<Result<Vec<_>, _>>()?;
        futures::future::try_join_all(
            tags.iter()
                .map(|tag| stores.tags.add_client_tag(&device.client_id, tag)),
        )
        .await?;

        let mut job = clauses;
        job.as_object_mut()
            .context("the clauses are a job object")?
            .insert("spec".to_string(), spec_bench_1());
        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("only.json".to_string(), job)],
        )
        .await?;

        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.status, status);
        assert_eq!(report.warnings.is_empty(), status == PlanStatus::Active);
        Ok(())
    }

    /// It names no job, so it is its own variant.
    #[tokio::test]
    async fn empty_job_set_is_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let err = ingest(&stores, &catalog()?, None, vec![])
            .await
            .err()
            .context("empty set is rejected")?;
        assert!(
            matches!(err, IngestError::Rejected(RejectionError::NoJobs)),
            "expected NoJobs, got {err:?}"
        );
        assert!(
            drain_plans(stores.plans.as_ref(), None)
                .await?
                .items
                .is_empty()
        );
        Ok(())
    }

    /// The job ceiling is checked on the whole set before anything is written. A
    /// set at the ceiling stages; one job past it is refused whole, and leaves
    /// no manifest and no queued job behind.
    #[rstest]
    #[case::at_the_ceiling(2, true)]
    #[case::past_the_ceiling(3, false)]
    #[tokio::test]
    async fn job_ceiling_is_checked_before_any_write(
        #[case] count: usize,
        #[case] staged: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let limit = std::num::NonZeroUsize::new(2).context("non-zero")?;
        let jobs = (0..count)
            .map(|i| {
                (
                    format!("job-{i}.json"),
                    cell_body(&format!("cell-{i}"), json!({})),
                )
            })
            .collect();

        let result = ingest_limited(
            &stores,
            &catalog()?,
            ModelCheck::none(),
            None,
            None,
            jobs,
            limit,
        )
        .await;

        let plans = drain_plans(stores.plans.as_ref(), None).await?.items;
        let queued = stores.todo.list_avail().await?;
        if staged {
            let report = result?;
            assert_eq!(report.job_count, count);
            assert_eq!(plans.len(), 1);
            assert_eq!(queued.len(), count);
            // One job source and one link for each job.
            assert_eq!(local_fs_job_record_count(dir.path()), 2 * count);
        } else {
            let err = result.err().context("a set past the ceiling is rejected")?;
            assert!(
                matches!(
                    err,
                    IngestError::Rejected(RejectionError::TooManyJobs { count: c, limit: l })
                        if c == count && l == limit
                ),
                "expected TooManyJobs, got {err:?}"
            );
            assert!(plans.is_empty(), "a refused set wrote a manifest");
            assert!(queued.is_empty(), "a refused set queued a job");
            assert_eq!(
                local_fs_job_record_count(dir.path()),
                0,
                "a refused set stored a job source or a link"
            );
        }
        Ok(())
    }

    /// The valid job sits first, so a rejection that lost the index would
    /// report `0` and pass a weaker assertion.
    #[tokio::test]
    async fn rejection_names_the_offending_job_by_index_and_label() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let jobs = two_job_set(json!({"requires":["OS:macos"],"spec":spec_bench_1()}));

        let err = ingest(&stores, &catalog()?, None, jobs)
            .await
            .err()
            .context("set is rejected")?;

        let IngestError::Rejected(RejectionError::Job { index, label, kind }) = err else {
            anyhow::bail!("expected a job rejection, got {err:?}");
        };
        assert_eq!(index, 1);
        assert_eq!(label, "bad.json");
        assert!(
            matches!(kind, JobRejectionError::NonCanonicalFlag { ref flag } if flag == "OS:macos"),
            "wrong rejection class: {kind:?}"
        );
        Ok(())
    }

    /// A vault reference outside the filer's scope is refused as unknown, by the
    /// index of the job naming it. The host CLI's context reaches every owner.
    #[tokio::test]
    async fn a_vault_reference_outside_the_filer_s_reach_is_refused_by_index() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let owner = VaultOwner::Org {
            org_id: pipette_plan_types::OrgId::try_new("org_0195ab")?,
        };
        let resource = pipette_store_testkit::vault_resource_for(
            owner.clone(),
            pipette_store_testkit::vault_blob_sha256()?,
            pipette_store_testkit::VAULT_BLOB.len() as u64,
        )?;
        stores
            .vault
            .put_blob(
                &resource.object_key,
                &resource.sha256,
                pipette_store_testkit::blob_body(pipette_store_testkit::VAULT_BLOB),
            )
            .await?;
        stores.vault.put_resource(&resource).await?;
        let model = json!({
            "source": "vault_url",
            "url": format!("/vault/resources/{}/content", resource.resource_id),
            "principal_id": owner.label(),
        });
        let jobs = vec![(
            "0".to_string(),
            json!({"requires": ["os:macos"], "spec": {"benchmark": "bench-1", "model": model, "runtime": {}}}),
        )];
        let catalog = catalog()?;
        let host = IngestContext::from_stores(
            &stores,
            &catalog,
            ModelCheck::none(),
            MAX_JOBS,
            CREATING_MAX_AGE,
        );
        validate_submission(&host, &jobs, Utc::now()).await?;

        let filer = VaultScope::Own {
            user: UserId::try_new(USER_ADA)?,
            org: None,
        };
        let filed = IngestContext {
            vault_scope: &filer,
            ..host
        };
        let err = validate_submission(&filed, &jobs, Utc::now())
            .await
            .err()
            .context("a resource outside the filer's reach is refused")?;
        let IngestError::Rejected(RejectionError::Job {
            index,
            kind: JobRejectionError::UnknownVaultResource { resource_id },
            ..
        }) = err
        else {
            anyhow::bail!("expected an unknown-resource rejection, got {err:?}");
        };
        assert_eq!(index, 0);
        assert_eq!(resource_id, resource.resource_id);
        Ok(())
    }

    /// A recipe reads a vault model's origin, but only within the filer's
    /// reach: another tenant's record gives no alias, so no refusal can name
    /// what it was imported from.
    #[tokio::test]
    async fn a_vlm_eval_recipe_reads_no_vault_model_outside_the_filer_s_reach() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let org = pipette_plan_types::OrgId::try_new("org_0195ab")?;
        let owner = VaultOwner::Org {
            org_id: org.clone(),
        };
        let resource = pipette_core::vault::VaultResource {
            origin: Some(pipette_store_testkit::a_huggingface_origin()?),
            ..pipette_store_testkit::vault_resource_for(
                owner.clone(),
                pipette_store_testkit::vault_blob_sha256()?,
                pipette_store_testkit::VAULT_BLOB.len() as u64,
            )?
        };
        stores.vault.put_resource(&resource).await?;
        let mut job = vlm_job("vlm-mmstar-lfm", "LiquidAI", "LFM2.5-VL-3B-GGUF", None);
        job["spec"]["model"] = json!({
            "type": "gguf_text",
            "source": "vault_url",
            "url": format!("/vault/resources/{}/content", resource.resource_id),
            "principal_id": owner.label(),
        });
        let jobs = vec![("0".to_string(), job)];
        let catalog = vlm_eval_catalog(&[])?;
        let user = UserId::try_new(USER_ADA)?;
        for (scope, aliased) in [
            (VaultScope::Every, true),
            (
                VaultScope::Own {
                    user: user.clone(),
                    org: Some(org.clone()),
                },
                true,
            ),
            (VaultScope::Own { user, org: None }, false),
        ] {
            let aliases = model_aliases(
                stores.vault.as_ref(),
                &scope,
                ModelCheck::none(),
                &catalog,
                &jobs,
            )
            .await?;
            anyhow::ensure!(
                aliases.first().is_some_and(Option::is_some) == aliased,
                "{scope:?}: {aliases:?}"
            );
        }
        Ok(())
    }

    /// A valid job followed by `bad_body`, so a rejection always lands at index
    /// 1 and the set exercises the whole-set rule rather than a single job.
    fn two_job_set(bad_body: Value) -> Vec<(String, Value)> {
        vec![
            (
                "ok.json".to_string(),
                json!({"requires":["os:macos"],"spec":spec_bench_1()}),
            ),
            ("bad.json".to_string(), bad_body),
        ]
    }

    #[tokio::test]
    async fn rejected_set_writes_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        // The second job violates reserved-namespace uniqueness — the whole set
        // must be rejected with nothing written.
        let jobs = two_job_set(json!({"requires":["os:ios","os:android"],"spec":spec_bench_1()}));
        assert!(ingest(&stores, &catalog()?, None, jobs).await.is_err());

        assert!(
            drain_plans(stores.plans.as_ref(), None)
                .await?
                .items
                .is_empty()
        );
        assert!(stores.todo.list_avail().await?.is_empty());
        assert_eq!(local_fs_job_record_count(dir.path()), 0);
        Ok(())
    }

    // ── the duplicate-job merge (PIP-518) ───────────────────────────────────

    /// A body over `cell`, so two inputs describe the same benchmark cell iff
    /// they name the same one. `extra` is merged into the envelope, which is how
    /// a case varies eligibility or the deadline.
    fn cell_body(cell: &str, extra: Value) -> Value {
        let mut body = json!({"requires": ["os:macos"], "spec": spec_bench_1()});
        body["spec"]["model"] = json!({"name": cell});
        if let Some(fields) = extra.as_object() {
            for (key, value) in fields {
                body[key] = value.clone();
            }
        }
        body
    }

    /// `days` from now, cut to whole seconds so it survives an RFC 3339
    /// `expires_at` and an `avail/` key unchanged.
    fn in_days(days: i64) -> DateTime<Utc> {
        let at = Utc::now() + Duration::days(days);
        DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at)
    }

    /// `at` in the RFC 3339 form that a handoff `expires_at` carries.
    fn rfc3339(at: DateTime<Utc>) -> String {
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// The plans holding `job_id`, sorted so an assertion can name them.
    async fn members(stores: &Stores, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
        let mut plans: Vec<PlanId> = stores
            .plans
            .list_job_plan_markers(job_id)
            .await?
            .into_iter()
            .map(|(plan_id, _)| plan_id)
            .collect();
        plans.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(plans)
    }

    /// Two plans that want one cell share one job. Both manifests list it, both
    /// hold a membership, and the second report marks the merge.
    #[tokio::test]
    async fn two_plans_wanting_one_cell_share_one_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let jobs = || vec![("cell.json".to_string(), cell_body("a", json!({})))];

        let first = ingest(&stores, &catalog()?, None, jobs()).await?;
        let second = ingest(&stores, &catalog()?, None, jobs()).await?;

        let shared = first.job_ids[0].1.clone();
        assert_eq!(second.job_ids[0].1, shared, "the second plan joins the job");
        assert_eq!(first.merged_job_count, 0);
        assert_eq!(second.merged_job_count, 1);
        assert_eq!(second.merges.len(), 1);
        assert_eq!(second.merges[0].label, "cell.json");
        assert_eq!(second.merges[0].kind, MergeKind::AcrossPlans);
        // An across-plans merge counts in `job_count` and `merged_job_count`
        // both; `job_ids` holds one entry per input.
        assert_eq!(second.job_count, 1);
        assert_eq!(second.job_ids.len(), 1, "one entry per input");

        // One queued job, listed by both manifests, held by both plans.
        assert_eq!(stores.todo.list_avail().await?.len(), 1);
        for report in [&first, &second] {
            let manifest = stores
                .plans
                .get_plan(&report.plan_id)
                .await?
                .context("manifest written")?;
            assert_eq!(manifest.job_ids, vec![shared.clone()]);
            assert_eq!(manifest.status, PlanStatus::PendingClients);
        }
        let mut expected = vec![first.plan_id.clone(), second.plan_id.clone()];
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(members(&stores, &shared).await?, expected);
        Ok(())
    }

    /// A plan that joins a job links it, and leaves the job source that the
    /// minting plan stored. The joiner's body differs in `merge_duplicates`,
    /// which the content key leaves out, so a replaced source would show.
    #[tokio::test]
    async fn a_joining_plan_links_the_job_and_keeps_its_source() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let first = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("cell.json".to_string(), cell_body("a", json!({})))],
        )
        .await?;
        let shared = first.job_ids[0].1.clone();
        let stored = stores
            .todo
            .get_avail_by_job(&shared)
            .await?
            .context("the first plan queued the job")?;
        let second = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "cell.json".to_string(),
                cell_body("a", json!({"merge_duplicates": true})),
            )],
        )
        .await?;

        assert_eq!(second.job_ids[0].1, shared, "the second plan joins the job");
        assert_eq!(stores.plans.get_job_source(&shared).await?, Some(stored));
        let mut linked = stores.plans.list_job_plan_links(&shared).await?;
        linked.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut expected = vec![first.plan_id, second.plan_id];
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(linked, expected);
        // One job source and two links.
        assert_eq!(local_fs_job_record_count(dir.path()), 3);
        Ok(())
    }

    /// A plan that joins a job with no source, such as one minted before job
    /// sources existed, stores its own stamped body as the source.
    #[tokio::test]
    async fn a_join_onto_a_job_with_no_source_stores_the_joiners_body() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let incumbent = JobId::from_uuid(uuid::Uuid::now_v7());
        let deadline = ExpiresAt::At(in_days(10));
        seed_content_key(
            &stores,
            &content_digest(&cell_body("a", json!({}))),
            &incumbent,
            Utc::now(),
        )
        .await?;
        seed_avail(&stores, &incumbent, &cell_body("a", json!({})), deadline).await?;

        let joiner = cell_body("a", json!({"expires_at": rfc3339(in_days(20))}));
        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("cell.json".to_string(), joiner.clone())],
        )
        .await?;

        assert_eq!(report.job_ids[0].1, incumbent, "the plan joins the job");
        let mut expected = joiner;
        expected["job_id"] = json!(incumbent.as_str());
        expected["expires_at"] = json!(deadline.to_string());
        assert_eq!(
            stores.plans.get_job_source(&incumbent).await?,
            Some(expected)
        );
        assert_eq!(
            stores.plans.list_job_plan_links(&incumbent).await?,
            vec![report.plan_id]
        );
        Ok(())
    }

    /// A difference in any eligibility clause keeps two jobs apart, while the
    /// same eligibility written in another order still merges.
    #[rstest]
    // The same sets, reordered: one job.
    #[case::reordered_requires(
        json!({"requires": ["os:macos", "runtime:mlx"]}),
        json!({"requires": ["runtime:mlx", "os:macos"]}),
        true
    )]
    #[case::reordered_any_of(
        json!({"requires": ["os:macos"], "any_of": [["a", "b"], ["c", "d"]]}),
        json!({"requires": ["os:macos"], "any_of": [["d", "c"], ["b", "a"]]}),
        true
    )]
    #[case::reordered_clients(
        json!({"clients": ["ev1_a", "ev1_b"]}),
        json!({"clients": ["ev1_b", "ev1_a"]}),
        true
    )]
    #[case::reordered_not_all_of(
        json!({"requires": ["os:macos"], "not_all_of": [["a", "b"], ["c", "d"]]}),
        json!({"requires": ["os:macos"], "not_all_of": [["d", "c"], ["b", "a"]]}),
        true
    )]
    // A different eligibility set: two jobs.
    #[case::different_requires(
        json!({"requires": ["os:macos"]}),
        json!({"requires": ["os:ios"]}),
        false
    )]
    #[case::extra_requires(
        json!({"requires": ["os:macos"]}),
        json!({"requires": ["os:macos", "runtime:mlx"]}),
        false
    )]
    #[case::added_any_of(
        json!({"requires": ["os:macos"]}),
        json!({"requires": ["os:macos"], "any_of": [["a", "b"]]}),
        false
    )]
    #[case::added_clients(
        json!({"requires": ["os:macos"]}),
        json!({"requires": ["os:macos"], "clients": ["ev1_a"]}),
        false
    )]
    // The negatives narrow eligibility, so a merge across one of them would
    // hand a job to a client its author excluded.
    #[case::added_excludes(
        json!({"requires": ["os:macos"]}),
        json!({"requires": ["os:macos"], "excludes": ["chip:a15"]}),
        false
    )]
    #[case::added_not_all_of(
        json!({"requires": ["os:macos"]}),
        json!({"requires": ["os:macos"], "not_all_of": [["chip:a15", "runtime:mlx"]]}),
        false
    )]
    #[case::added_exclude_clients(
        json!({"clients": ["ev1_a", "ev1_b"]}),
        json!({"clients": ["ev1_a", "ev1_b"], "exclude_clients": ["ev1_b"]}),
        false
    )]
    #[tokio::test]
    async fn eligibility_decides_whether_two_cells_merge(
        #[case] first_eligibility: Value,
        #[case] second_eligibility: Value,
        #[case] merges: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let first = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("a.json".to_string(), cell_body("a", first_eligibility))],
        )
        .await?;
        let second = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("a.json".to_string(), cell_body("a", second_eligibility))],
        )
        .await?;

        assert_eq!(first.job_ids[0].1 == second.job_ids[0].1, merges);
        assert_eq!(second.merged_job_count, usize::from(merges));
        assert_eq!(
            stores.todo.list_avail().await?.len(),
            if merges { 1 } else { 2 }
        );
        Ok(())
    }

    // ── campaign revisions: carrying the superseded plan's jobs ─────────────

    /// Ingest `jobs` as one plan filed under `membership`, from a document when
    /// `source` names one.
    async fn ingest_under(
        stores: &Stores,
        membership: CampaignMembership,
        source: Option<PlanSource>,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
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
                membership,
                jobs,
                source,
                plan_warnings: Vec::new(),
            },
        )
        .await?)
    }

    /// A stand-in for the document `jobs` expand from: only a plan filed from
    /// one records `content_keys`.
    fn document() -> anyhow::Result<Option<PlanSource>> {
        Ok(Some(PlanSource::new("# the document\n".to_string())?))
    }

    /// A campaign created around `jobs` as its first plan, filed from a
    /// document.
    async fn campaign_of(
        stores: &Stores,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<(pipette_core::campaign::Campaign, IngestReport)> {
        let new = pipette_core::campaign::NewCampaign::new(
            NonEmptyTrimmedString::try_new("nightly")?,
            pipette_core::campaign::CampaignOwner::User {
                user_id: UserId::try_new(USER_ADA)?,
            },
        );
        let first =
            ingest_under(stores, CampaignMembership::first(&new), document()?, jobs).await?;
        Ok((new.around(first.plan_id.clone(), Utc::now()), first))
    }

    /// File `jobs` as a variant edit of `campaign` would: superseding the plan
    /// it runs, and carrying that plan's jobs over.
    async fn revise(
        stores: &Stores,
        campaign: &pipette_core::campaign::Campaign,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        rerun(stores, campaign, &BTreeSet::new(), jobs).await
    }

    /// [`revise`], running the `rerun` jobs of the plan `campaign` runs again.
    async fn rerun(
        stores: &Stores,
        campaign: &pipette_core::campaign::Campaign,
        rerun: &BTreeSet<JobId>,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        revise_naming(stores, campaign, rerun, &BTreeSet::new(), jobs).await
    }

    /// [`revise`], placing the `rerun` and `recalculate` jobs of the plan
    /// `campaign` runs afresh.
    async fn revise_naming(
        stores: &Stores,
        campaign: &pipette_core::campaign::Campaign,
        rerun: &BTreeSet<JobId>,
        recalculate: &BTreeSet<JobId>,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        let head = campaign.current_plan_id.as_ref().context("a head")?;
        let previous = stores.plans.get_plan(head).await?.context("the head")?;
        ingest_under(
            stores,
            CampaignMembership::carrying(campaign, &previous, rerun, recalculate),
            document()?,
            jobs,
        )
        .await
    }

    fn cells(names: &[&str], extra: Value) -> Vec<(String, Value)> {
        names
            .iter()
            .map(|name| (format!("{name}.json"), cell_body(name, extra.clone())))
            .collect()
    }

    /// Only a plan filed from a document records each job's cell, which a later
    /// revision matches on.
    #[tokio::test]
    async fn a_campaign_plan_records_the_content_key_of_each_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let (campaign, first) = campaign_of(&stores, cells(&["a", "b"], json!({}))).await?;
        let manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("manifest")?;
        let expected: BTreeMap<JobId, String> = first
            .job_ids
            .iter()
            .zip(["a", "b"])
            .map(|((_, job_id), name)| {
                (job_id.clone(), content_digest(&cell_body(name, json!({}))))
            })
            .collect();
        assert_eq!(manifest.content_keys, expected);

        let elsewhere = ingest(&stores, &catalog()?, None, cells(&["c"], json!({}))).await?;
        let bodies = ingest_under(
            &stores,
            CampaignMembership::of(&campaign),
            None,
            cells(&["d"], json!({})),
        )
        .await?;
        let manifests = futures::future::try_join_all(
            [&elsewhere.plan_id, &bodies.plan_id].map(|plan_id| stores.plans.get_plan(plan_id)),
        )
        .await?;
        assert!(
            manifests
                .iter()
                .all(|manifest| manifest.as_ref().is_some_and(|m| m.content_keys.is_empty())),
            "{manifests:?}"
        );
        Ok(())
    }

    /// A finished job is carried but not queued again, and the fleet match does
    /// not count it; only the new cells mint.
    #[tokio::test]
    async fn a_revision_carries_the_cells_it_keeps_finished_or_not() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a", "b"], json!({}))).await?;
        let (done, live) = (first.job_ids[0].1.clone(), first.job_ids[1].1.clone());
        finish_job(&*stores.submissions, &*stores.todo, &done).await?;

        let revision = revise(&stores, &campaign, cells(&["a", "b", "c"], json!({}))).await?;

        assert_eq!(revision.job_ids[0].1, done);
        assert_eq!(revision.job_ids[1].1, live);
        let minted = revision.job_ids[2].1.clone();
        assert!(minted != done && minted != live);
        let kinds = merge_kinds(&revision);
        assert_eq!(kinds, vec![MergeKind::Carried, MergeKind::Carried]);

        // The finished job stays out of the queue, and the revision writes no
        // marker on it: there is nothing left of it to lapse or tear down.
        assert_eq!(stores.todo.list_avail().await?.len(), 2);
        assert_eq!(members(&stores, &done).await?, vec![first.plan_id.clone()]);
        let mut both = vec![first.plan_id.clone(), revision.plan_id.clone()];
        both.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(members(&stores, &live).await?, both);

        // No client is registered, so the two outstanding jobs starve and the
        // finished one is not among them.
        let manifest = stores
            .plans
            .get_plan(&revision.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.status, PlanStatus::PendingClients);
        assert_eq!(manifest.warnings.len(), 1);
        assert_eq!(manifest.warnings[0].job_ids, vec![live, minted]);
        assert_eq!(manifest.previous_plan_id, Some(first.plan_id));
        assert_eq!(manifest.content_keys.len(), 3);
        Ok(())
    }

    /// A revision of finished work has nothing to wait on, so it reads `active`
    /// with no warning until `queue-maintenance` latches it complete, where a
    /// starved plan would read `pending_clients`.
    #[tokio::test]
    async fn a_revision_of_finished_work_waits_on_no_client() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], json!({}))).await?;
        finish_job(&*stores.submissions, &*stores.todo, &first.job_ids[0].1).await?;

        let revision = revise(&stores, &campaign, cells(&["a"], json!({}))).await?;

        let manifest = stores
            .plans
            .get_plan(&revision.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.job_ids, vec![first.job_ids[0].1.clone()]);
        assert_eq!(manifest.status, PlanStatus::Active);
        assert!(manifest.warnings.is_empty());
        Ok(())
    }

    /// Opting out of merging keeps a plan off other plans' runs of a cell, and
    /// the superseded plan's run is this campaign's own, so it is carried all
    /// the same.
    #[tokio::test]
    async fn a_revision_carries_an_opted_out_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let opted_out = || cells(&["a"], json!({"merge_duplicates": false}));
        let (campaign, first) = campaign_of(&stores, opted_out()).await?;

        let revision = revise(&stores, &campaign, opted_out()).await?;

        assert_eq!(revision.job_ids[0].1, first.job_ids[0].1);
        assert_eq!(revision.merges[0].kind, MergeKind::Carried);
        assert_eq!(stores.todo.list_avail().await?.len(), 1);
        Ok(())
    }

    /// [`revise`] as a filed revision reads the head: with the body each of its
    /// jobs was filed under.
    async fn revise_reading_sources(
        stores: &Stores,
        campaign: &pipette_core::campaign::Campaign,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        ingest_under(
            stores,
            carrying_sources(stores, campaign).await?,
            document()?,
            jobs,
        )
        .await
    }

    /// [`draw_carry`] over `jobs` as ingestion stages them.
    async fn preview_draw(
        stores: &Stores,
        campaign: &pipette_core::campaign::Campaign,
        jobs: &[(String, Value)],
    ) -> anyhow::Result<CarryDraw> {
        let mut staged = jobs.to_vec();
        lift_required(&mut staged)?;
        let bodies: Vec<Value> = staged.into_iter().map(|(_, body)| body).collect();
        Ok(draw_carry(
            carrying_sources(stores, campaign).await?,
            &bodies,
            &*stores.plans,
            &*stores.submissions,
            &*stores.orgs,
        )
        .await?)
    }

    /// What [`revise_reading_sources`] files under.
    async fn carrying_sources(
        stores: &Stores,
        campaign: &pipette_core::campaign::Campaign,
    ) -> anyhow::Result<CampaignMembership> {
        let head = campaign.current_plan_id.as_ref().context("a head")?;
        let previous = stores.plans.get_plan(head).await?.context("the head")?;
        let sources: HashMap<JobId, Value> = futures::stream::iter(previous.content_keys.keys())
            .then(|job_id| async move {
                let source = stores.plans.get_job_source(job_id).await?;
                anyhow::Ok((job_id.clone(), source.context("a job source")?))
            })
            .try_collect()
            .await?;
        let mut membership =
            CampaignMembership::carrying(campaign, &previous, &BTreeSet::new(), &BTreeSet::new());
        membership.carry = membership.carry.with_sources(sources);
        Ok(membership)
    }

    /// A success whose device still meets the new clauses is carried; a failure
    /// and a queued job run again, and the preview counts the same.
    #[tokio::test]
    async fn a_requirement_change_carries_a_run_that_still_answers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let pinned = json!({"requires": ["os:macos", "client_version:2026.10.05-438185c"]});
        let (campaign, first) = campaign_of(&stores, cells(&["a", "b", "c"], pinned)).await?;
        let [(_, done), (_, failed), (_, queued)] = <[(String, JobId); 3]>::try_from(first.job_ids)
            .map_err(|jobs| anyhow::anyhow!("three jobs, got {jobs:?}"))?;
        finish_job_with_success(&*stores.submissions, &*stores.todo, &done, "prefill").await?;
        finish_job_with_failure(&*stores.submissions, &*stores.todo, &failed).await?;
        // The recorded device is an Apple M5 MacBook Pro.
        let rerouted = json!({"requires": ["os:macos", "chip:applem5", "tag:lab-east"]});
        let jobs = cells(&["a", "b", "c"], rerouted);

        let draw = preview_draw(&stores, &campaign, &jobs).await?;
        assert_eq!(
            draw,
            CarryDraw {
                carried: vec![done.clone()],
                new_cells: 1,
                requirements_rerun: 1,
                satisfied: 0,
                offered: HashMap::new(),
                held: vec![HeldJob {
                    body_index: 0,
                    plan_id: first.plan_id.clone(),
                    job_id: done.clone(),
                }],
            }
        );

        let revision = revise_reading_sources(&stores, &campaign, jobs).await?;

        assert_eq!(revision.job_ids[0].1, done);
        assert_eq!(revision.merges.len(), 1);
        assert_eq!(revision.merges[0].kind, MergeKind::Carried);
        assert!(![&done, &failed, &queued].contains(&&revision.job_ids[1].1));
        assert!(![&done, &failed, &queued].contains(&&revision.job_ids[2].1));
        let manifest = stores
            .plans
            .get_plan(&revision.plan_id)
            .await?
            .context("manifest")?;
        // Keyed under the clauses it now answers, so the next revision carries
        // it by digest.
        assert_eq!(
            manifest.content_keys.get(&done),
            Some(&content_digest(&cell_body(
                "a",
                json!({"requires": ["os:macos", "chip:applem5", "tag:lab-east"]})
            )))
        );
        assert_eq!(members(&stores, &done).await?, vec![first.plan_id.clone()]);
        Ok(())
    }

    /// What a revision does with one finished success, recorded on an Apple M5
    /// MacBook Pro ([`pipette_store_testkit::success_record`]) at `build`, when
    /// the cell's clauses go from `before` to `after`. The dry run and the
    /// filing agree on it.
    #[rstest]
    // An unmarked build only routes.
    #[case::pin_moved(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos"], "any_of": [["client_version:new"]]}), Some("old"), true)]
    #[case::pin_moved_unknown_build(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos"], "any_of": [["client_version:new"]]}), None, true)]
    #[case::build_excluded(json!({"requires": ["os:macos"]}), json!({"requires": ["os:macos"], "excludes": ["client_version:old"]}), Some("old"), true)]
    // A required build must be the run's.
    #[case::required_pin_moved(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos"], "any_of": [["client_version:new:required"]]}), Some("old"), false)]
    #[case::required_pin_moved_to_its_build(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos"], "any_of": [["client_version:new:required"]]}), Some("new"), true)]
    #[case::required_pin_moved_unknown_build(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos"], "any_of": [["client_version:new:required"]]}), None, false)]
    #[case::required_pin_widened(json!({"requires": ["os:macos"], "any_of": [["client_version:old:required"]]}), json!({"requires": ["os:macos"], "any_of": [["client_version:new:required", "client_version:old:required"]]}), Some("old"), true)]
    #[case::required_pin_marked_later(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos", "tag:lab-east"], "any_of": [["client_version:old:required"]]}), Some("new"), false)]
    #[case::required_pin_marked_later_on_its_build(json!({"requires": ["os:macos"], "any_of": [["client_version:old"]]}), json!({"requires": ["os:macos", "tag:lab-east"], "any_of": [["client_version:old:required"]]}), Some("old"), true)]
    #[case::required_pin_kept_beside_a_tag(json!({"requires": ["os:macos"], "any_of": [["client_version:old:required"]]}), json!({"requires": ["os:macos", "tag:lab-east"], "any_of": [["client_version:old:required"]]}), None, true)]
    #[case::required_pin_dropped(json!({"requires": ["os:macos"], "any_of": [["client_version:old:required"]]}), json!({"requires": ["os:macos", "tag:lab-east"]}), None, true)]
    #[case::required_pin_in_requires(json!({"requires": ["os:macos"]}), json!({"requires": ["os:macos", "client_version:old:required"]}), Some("old"), true)]
    #[case::required_device_missed(json!({"requires": ["os:macos"], "any_of": [["client_version:old:required"]]}), json!({"requires": ["os:macos", "chip:applem3"], "any_of": [["client_version:old:required"]]}), Some("old"), false)]
    // A required tag is known only where the old body required it.
    #[case::a_tag_it_was_required_to_hold(json!({"requires": ["os:macos", "tag:lab-east"]}), json!({"requires": ["os:macos", "tag:lab-east:required"]}), None, true)]
    #[case::a_new_required_tag(json!({"requires": ["os:macos"]}), json!({"requires": ["os:macos", "tag:lab-east:required"]}), None, false)]
    // The device: a flag the record shows carries.
    #[case::a_chip_it_has(json!({"requires": ["os:macos"]}), json!({"requires": ["os:macos", "chip:applem5"]}), None, true)]
    #[case::a_chip_it_lacks(json!({"requires": ["os:macos"]}), json!({"requires": ["os:macos", "chip:applem3"]}), None, false)]
    #[case::a_chip_excluded(json!({"requires": ["os:macos"]}), json!({"requires": ["os:macos"], "excludes": ["chip:applem5"]}), None, false)]
    // A roster run carries on what its record shows.
    #[case::a_roster_run_its_device_meets(json!({"clients": ["ev1_a"], "requires": ["os:macos", "chip:applem5"]}), json!({"requires": ["os:macos", "chip:applem5"]}), None, true)]
    #[tokio::test]
    async fn a_revision_carries_a_finished_run_it_knows_answers(
        #[case] before: Value,
        #[case] after: Value,
        #[case] build: Option<&str>,
        #[case] carried: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], before)).await?;
        let (_, done) = first.job_ids.first().context("one job")?.clone();
        finish_job_with_success_on_build(
            &*stores.submissions,
            &*stores.todo,
            &done,
            "prefill",
            build,
        )
        .await?;
        let jobs = cells(&["a"], after);

        let draw = preview_draw(&stores, &campaign, &jobs).await?;
        let revision = revise_reading_sources(&stores, &campaign, jobs).await?;

        let expected = if carried {
            CarryDraw {
                carried: vec![done.clone()],
                new_cells: 0,
                requirements_rerun: 0,
                satisfied: 0,
                offered: HashMap::new(),
                held: vec![HeldJob {
                    body_index: 0,
                    plan_id: first.plan_id.clone(),
                    job_id: done.clone(),
                }],
            }
        } else {
            CarryDraw {
                carried: Vec::new(),
                new_cells: 0,
                requirements_rerun: 1,
                satisfied: 0,
                offered: HashMap::new(),
                held: Vec::new(),
            }
        };
        assert_eq!(draw, expected);
        let (_, placed) = revision.job_ids.first().context("one job")?;
        assert_eq!(*placed == done, carried);
        Ok(())
    }

    /// Ingestion takes `:required` off a tag or a build and lists the plain flag
    /// in `required_flags`; a body that marks neither stays as it was.
    #[rstest]
    #[case::nothing_marked(json!({"requires": ["os:macos"]}), Ok(json!({"requires": ["os:macos"]})))]
    #[case::marked(
        json!({"requires": ["os:macos", "tag:lab-east:required"], "any_of": [["client_version:b:required", "client_version:a:required"]]}),
        Ok(json!({"requires": ["os:macos", "tag:lab-east"], "any_of": [["client_version:b", "client_version:a"]], "required_flags": ["client_version:a", "client_version:b", "tag:lab-east"]}))
    )]
    #[case::preset(json!({"requires": ["os:macos"], "required_flags": []}), Err("carries `required_flags`"))]
    // Any other flag is measured already, and one that ends so stays itself.
    #[case::not_a_routing_flag(
        json!({"requires": ["os:macos:required", "feature:required", ":required"], "excludes": ["chip:a15:required"]}),
        Ok(json!({"requires": ["os:macos:required", "feature:required", ":required"], "excludes": ["chip:a15:required"]}))
    )]
    #[case::in_excludes(json!({"requires": ["os:macos"], "excludes": ["tag:flaky:required"]}), Err("in `excludes`"))]
    #[case::in_not_all_of(json!({"requires": ["os:macos"], "not_all_of": [["tag:flaky:required", "os:ios"]]}), Err("in `not_all_of`"))]
    #[case::partly_marked_group(json!({"requires": ["os:macos"], "any_of": [["client_version:a:required", "client_version:b"]]}), Err("marks some"))]
    #[case::no_tag(json!({"requires": ["os:macos", "tag::required"]}), Err("names no tag or build"))]
    #[case::no_build(json!({"requires": ["os:macos"], "any_of": [["client_version::required"]]}), Err("names no tag or build"))]
    fn ingestion_lifts_the_required_mark(
        #[case] body: Value,
        #[case] expected: Result<Value, &str>,
    ) {
        let mut lifted = body;
        let got = lift_required_in(&mut lifted)
            .map(|()| lifted)
            .map_err(|e| e.to_string());
        match expected {
            Ok(expected) => assert_eq!(got, Ok(expected)),
            Err(fragment) => assert!(got.as_ref().is_err_and(|e| e.contains(fragment)), "{got:?}"),
        }
    }

    /// A job whose horizon lapsed for the superseded plan never answered it, so
    /// the revision does not carry it: the cell is placed by the ordinary rules,
    /// which here join the job while it is still queued.
    #[tokio::test]
    async fn a_lapsed_job_is_placed_as_any_other_cell() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], json!({}))).await?;
        let job_id = first.job_ids[0].1.clone();
        let mut manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("manifest")?;
        manifest.expired_jobs.insert(
            job_id.clone(),
            pipette_core::plan::ExpiredJob {
                horizon: in_days(-1),
                recorded_at: Utc::now(),
            },
        );
        stores.plans.put_plan(&manifest).await?;

        let revision = revise(&stores, &campaign, cells(&["a"], json!({}))).await?;

        assert_eq!(revision.job_ids[0].1, job_id);
        assert_eq!(revision.merges[0].kind, MergeKind::AcrossPlans);
        Ok(())
    }

    /// Claim `job_id` as a client does: it moves to `leased/`, and runs there.
    async fn lease(stores: &Stores, job_id: &JobId) -> anyhow::Result<()> {
        let name = stores
            .todo
            .avail_key_for_job(job_id)
            .await?
            .context("the job is queued")?;
        let (_, expires_at) = parse_avail_filename(&name)?;
        let claimed = stores
            .todo
            .claim_job(
                job_id,
                expires_at,
                &ClientId::try_new("ev1_a")?,
                Utc::now() + Duration::hours(1),
            )
            .await?;
        anyhow::ensure!(
            matches!(claimed, ClaimResult::Claimed(_)),
            "{job_id} was not claimed"
        );
        Ok(())
    }

    /// A running job is carried as a queued one is: the revision writes its
    /// membership marker, so the superseded plan's teardown releases the lease
    /// rather than retiring the job under the client running it.
    #[tokio::test]
    async fn a_revision_carries_a_running_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], json!({}))).await?;
        let running = first.job_ids[0].1.clone();
        lease(&stores, &running).await?;

        let revision = revise(&stores, &campaign, cells(&["a"], json!({}))).await?;

        assert_eq!(revision.job_ids[0].1, running);
        assert_eq!(revision.merges[0].kind, MergeKind::Carried);
        let mut both = vec![first.plan_id.clone(), revision.plan_id.clone()];
        both.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(members(&stores, &running).await?, both);
        Ok(())
    }

    /// A submission records its result before it tears its lease down. A job
    /// caught between the two is finished: the revision takes it with no
    /// marker, and waits on no client for it.
    #[tokio::test]
    async fn a_recorded_job_is_finished_while_its_lease_stands() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], json!({}))).await?;
        let job_id = first.job_ids[0].1.clone();
        lease(&stores, &job_id).await?;
        stores
            .submissions
            .write_processed(&job_id, &json!({"job_id": job_id.as_str()}))
            .await?;

        let revision = revise(&stores, &campaign, cells(&["a"], json!({}))).await?;

        assert_eq!(revision.job_ids[0].1, job_id);
        assert_eq!(revision.merges[0].kind, MergeKind::Carried);
        assert_eq!(
            members(&stores, &job_id).await?,
            vec![first.plan_id.clone()]
        );
        let manifest = stores
            .plans
            .get_plan(&revision.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.status, PlanStatus::Active);
        assert!(manifest.warnings.is_empty());
        Ok(())
    }

    /// How a job the superseded plan held can be gone with no result.
    #[derive(Clone, Copy, Debug)]
    enum Gone {
        Retired,
        /// Still queued, under a deadline that has passed.
        PastDeadline,
    }

    /// A job the revision finds gone with no result is not carried. The cell is
    /// placed as any other, which here mints a fresh job.
    #[rstest]
    #[case::retired(Gone::Retired)]
    #[case::past_its_deadline(Gone::PastDeadline)]
    #[tokio::test]
    async fn a_job_gone_without_a_result_is_not_carried(#[case] gone: Gone) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], json!({}))).await?;
        let job_id = first.job_ids[0].1.clone();
        match gone {
            Gone::Retired => stores.todo.delete_avail_by_job(&job_id).await?,
            Gone::PastDeadline => {
                let name = stores
                    .todo
                    .avail_key_for_job(&job_id)
                    .await?
                    .context("the job is queued")?;
                let (_, expires_at) = parse_avail_filename(&name)?;
                let redated = stores
                    .todo
                    .redate_avail(&job_id, expires_at, ExpiresAt::At(in_days(-1)))
                    .await?;
                anyhow::ensure!(redated == pipette_store::RedateResult::Redated);
            }
        }

        let revision = revise(&stores, &campaign, cells(&["a"], json!({}))).await?;

        assert_ne!(revision.job_ids[0].1, job_id);
        assert_eq!(revision.merges, Vec::new(), "{gone:?}");
        Ok(())
    }

    /// A plan that opted out ran one cell twice, as two jobs. A revision that
    /// runs it twice takes one job per run, and one that runs it once takes one
    /// of the two.
    #[tokio::test]
    async fn each_opted_out_run_of_a_cell_carries_its_own_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let opted_out = |labels: &[&str]| -> Vec<(String, Value)> {
            labels
                .iter()
                .map(|label| {
                    let body = cell_body("a", json!({"merge_duplicates": false}));
                    (format!("{label}.json"), body)
                })
                .collect()
        };
        let (campaign, first) = campaign_of(&stores, opted_out(&["a1", "a2"])).await?;
        let ran: BTreeSet<JobId> = first
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        assert_eq!(ran.len(), 2);

        let twice = revise(&stores, &campaign, opted_out(&["a1", "a2"])).await?;
        let carried: BTreeSet<JobId> = twice
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        assert_eq!(carried, ran);

        let once = revise(&stores, &campaign, opted_out(&["a1"])).await?;
        assert!(ran.contains(&once.job_ids[0].1));

        // Re-running one of the two takes the other, and mints the second run.
        let first_run = first.job_ids[0].1.clone();
        let rerun_once = rerun(
            &stores,
            &campaign,
            &BTreeSet::from([first_run.clone()]),
            opted_out(&["a1", "a2"]),
        )
        .await?;
        let held: BTreeSet<JobId> = rerun_once
            .job_ids
            .iter()
            .map(|(_, job_id)| job_id.clone())
            .collect();
        assert_eq!(held.len(), 2);
        assert!(!held.contains(&first_run));
        assert!(held.contains(&first.job_ids[1].1));
        Ok(())
    }

    /// A re-run carries every cell but the one it names, which it mints afresh
    /// and queues, so a finished cell runs again under a new job.
    #[tokio::test]
    async fn a_rerun_mints_the_cell_it_names_and_carries_the_rest() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a", "b"], json!({}))).await?;
        let (again, kept) = (first.job_ids[0].1.clone(), first.job_ids[1].1.clone());
        finish_job(&*stores.submissions, &*stores.todo, &again).await?;
        finish_job(&*stores.submissions, &*stores.todo, &kept).await?;

        let revision = rerun(
            &stores,
            &campaign,
            &BTreeSet::from([again.clone()]),
            cells(&["a", "b"], json!({})),
        )
        .await?;

        let minted = revision.job_ids[0].1.clone();
        assert_ne!(minted, again);
        assert_eq!(revision.job_ids[1].1, kept);
        let kinds = merge_kinds(&revision);
        assert_eq!(kinds, vec![MergeKind::Carried]);
        assert!(stores.todo.get_avail_by_job(&minted).await?.is_some());
        assert_eq!(members(&stores, &minted).await?, vec![revision.plan_id]);
        Ok(())
    }

    /// A submission records its result before it tears its lease down, so an
    /// answered job can still read as live, and the index still names it. A
    /// re-run mints rather than joining it, which would run nothing.
    #[tokio::test]
    async fn a_rerun_does_not_join_the_job_it_runs_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a"], json!({}))).await?;
        let job_id = first.job_ids[0].1.clone();
        lease(&stores, &job_id).await?;
        stores
            .submissions
            .write_processed(&job_id, &json!({"job_id": job_id.as_str()}))
            .await?;

        let revision = rerun(
            &stores,
            &campaign,
            &BTreeSet::from([job_id.clone()]),
            cells(&["a"], json!({})),
        )
        .await?;

        assert_ne!(revision.job_ids[0].1, job_id);
        assert_eq!(revision.merges, Vec::new());
        Ok(())
    }

    /// A re-run's manifest names the job each minted job runs again; a later
    /// revision keeps the name, and a re-run of the re-run names the re-run.
    #[rstest]
    #[case::variant_edit(false)]
    #[case::recalculation(true)]
    #[tokio::test]
    async fn a_rerun_names_the_job_it_runs_again(
        #[case] recalculates_kept: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (mut campaign, first) = campaign_of(&stores, cells(&["a", "b"], json!({}))).await?;
        let (again, kept) = (first.job_ids[0].1.clone(), first.job_ids[1].1.clone());
        finish_job(&*stores.submissions, &*stores.todo, &again).await?;
        finish_job_with_success(&*stores.submissions, &*stores.todo, &kept, "eval").await?;
        let rerun_of = |plan_id: PlanId| {
            let plans = &stores.plans;
            async move {
                let manifest = plans.get_plan(&plan_id).await?.context("the plan")?;
                anyhow::Ok(manifest.rerun_of)
            }
        };

        let revision = rerun(
            &stores,
            &campaign,
            &BTreeSet::from([again.clone()]),
            cells(&["a", "b"], json!({})),
        )
        .await?;
        let minted = revision.job_ids[0].1.clone();
        assert_eq!(
            rerun_of(revision.plan_id.clone()).await?,
            BTreeMap::from([(minted.clone(), again.clone())])
        );

        campaign.advance(revision.plan_id, Utc::now());
        let recalculate = if recalculates_kept {
            BTreeSet::from([kept.clone()])
        } else {
            BTreeSet::new()
        };
        let carried = revise_naming(
            &stores,
            &campaign,
            &BTreeSet::new(),
            &recalculate,
            cells(&["a", "b"], json!({})),
        )
        .await?;
        assert_eq!(carried.job_ids[0].1, minted);
        assert_eq!(carried.job_ids[1].1 != kept, recalculates_kept);
        assert_eq!(
            rerun_of(carried.plan_id.clone()).await?,
            BTreeMap::from([(minted.clone(), again)])
        );

        finish_job(&*stores.submissions, &*stores.todo, &minted).await?;
        campaign.advance(carried.plan_id, Utc::now());
        let twice = rerun(
            &stores,
            &campaign,
            &BTreeSet::from([minted.clone()]),
            cells(&["a", "b"], json!({})),
        )
        .await?;
        assert_eq!(
            rerun_of(twice.plan_id.clone()).await?,
            BTreeMap::from([(twice.job_ids[0].1.clone(), minted)])
        );
        Ok(())
    }

    /// Opted-out twins a re-run names each mint a job naming its own run.
    #[tokio::test]
    async fn a_rerun_of_opted_out_twins_names_each_run() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let twins = || cells(&["a", "a"], json!({"merge_duplicates": false}));
        let (campaign, first) = campaign_of(&stores, twins()).await?;
        let runs: BTreeSet<JobId> = first.job_ids.iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(runs.len(), 2, "the twins run apart");
        for job_id in &runs {
            finish_job(&*stores.submissions, &*stores.todo, job_id).await?;
        }

        let revision = rerun(&stores, &campaign, &runs, twins()).await?;

        let manifest = stores
            .plans
            .get_plan(&revision.plan_id)
            .await?
            .context("the revision")?;
        let minted: BTreeSet<JobId> = revision.job_ids.iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(
            manifest.rerun_of.keys().cloned().collect::<BTreeSet<_>>(),
            minted
        );
        assert_eq!(
            manifest.rerun_of.values().cloned().collect::<BTreeSet<_>>(),
            runs
        );
        Ok(())
    }

    /// A re-run or recalculation whose document now places other cells than
    /// the head's is refused before anything is written, dry run or not.
    #[rstest]
    #[case::unchanged(&["a", "b"], json!({}), &["a", "b"], json!({}), None)]
    #[case::a_cell_changed(&["a", "b"], json!({}), &["a", "c"], json!({}), Some(1))]
    #[case::the_named_cell_is_gone(&["a", "b"], json!({}), &["b"], json!({}), Some(1))]
    #[case::opted_out_twins(
        &["a", "a"],
        json!({"merge_duplicates": false}),
        &["a", "a"],
        json!({"merge_duplicates": false}),
        None
    )]
    #[case::opted_out_twins_now_merge(
        &["a", "a"],
        json!({"merge_duplicates": false}),
        &["a", "a"],
        json!({}),
        Some(1)
    )]
    #[tokio::test]
    async fn a_drifted_revision_is_refused_by_the_check_and_the_ingest(
        #[case] head: &[&str],
        #[case] head_extra: Value,
        #[case] now: &[&str],
        #[case] now_extra: Value,
        #[case] drift: Option<usize>,
        #[values(false, true)] recalculated: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(head, head_extra)).await?;
        let job_id = first.job_ids[0].1.clone();
        finish_job_with_success(&*stores.submissions, &*stores.todo, &job_id, "eval").await?;
        let (rerun, recalculate) = match recalculated {
            false => (BTreeSet::from([job_id]), BTreeSet::new()),
            true => (BTreeSet::new(), BTreeSet::from([job_id])),
        };
        let previous = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("the head")?;
        let jobs = cells(now, now_extra);
        let bodies: Vec<Value> = jobs.iter().map(|(_, body)| body.clone()).collect();

        let checked = draw_carry(
            CampaignMembership::carrying(&campaign, &previous, &rerun, &recalculate),
            &bodies,
            &*stores.plans,
            &*stores.submissions,
            &*stores.orgs,
        )
        .await;
        let filed = revise_naming(&stores, &campaign, &rerun, &recalculate, jobs).await;

        match drift {
            None => {
                checked?;
                filed?;
            }
            Some(expected) => {
                assert!(
                    matches!(
                        checked,
                        Err(IngestError::Rejected(RejectionError::RerunDrift { cells })) if cells == expected
                    ),
                    "{checked:?}"
                );
                let refused = filed.err().context("a drifted revision is refused")?;
                assert!(
                    matches!(
                        refused.downcast_ref::<IngestError>(),
                        Some(IngestError::Rejected(RejectionError::RerunDrift { cells })) if *cells == expected
                    ),
                    "{refused:#}"
                );
                assert_eq!(drain_plans(&*stores.plans, None).await?.items.len(), 1);
            }
        }
        Ok(())
    }

    /// A recalculated cell two inputs share is answered once: the input that
    /// mints the new job writes the one copy, and its twin joins that job.
    #[tokio::test]
    async fn a_recalculation_answers_a_shared_cell_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (campaign, first) = campaign_of(&stores, cells(&["a", "a"], json!({}))).await?;
        let named = first.job_ids[0].1.clone();
        assert_eq!(first.job_ids[1].1, named, "the twins share one job");
        finish_job_with_success(&*stores.submissions, &*stores.todo, &named, "eval").await?;

        let revision = revise_naming(
            &stores,
            &campaign,
            &BTreeSet::new(),
            &BTreeSet::from([named.clone()]),
            cells(&["a", "a"], json!({})),
        )
        .await?;

        let fresh = revision.job_ids[0].1.clone();
        assert_ne!(fresh, named);
        assert_eq!(revision.job_ids[1].1, fresh);
        assert_eq!(
            stores.submissions.list_incoming(NonZeroUsize::MAX).await?,
            vec![fresh.clone()]
        );
        assert!(stores.todo.get_avail_by_job(&fresh).await?.is_none());
        Ok(())
    }

    /// Opting out is a mint-time rule: it does not make the job private, so a
    /// later plan borrows it.
    #[tokio::test]
    async fn an_opted_out_job_mints_its_own_and_still_lends_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let opted_out = || {
            vec![(
                "cell.json".to_string(),
                cell_body("a", json!({"merge_duplicates": false})),
            )]
        };

        // Two opted-out ingests of one cell: two jobs, because neither reads the
        // index. The first published the key, so the second's claim finds it
        // taken and leaves the incumbent alone.
        let first = ingest(&stores, &catalog()?, None, opted_out()).await?;
        let second = ingest(&stores, &catalog()?, None, opted_out()).await?;
        assert_ne!(first.job_ids[0].1, second.job_ids[0].1);
        assert_eq!(second.merged_job_count, 0);
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(keys.len(), 1, "one key names the cell");
        assert_eq!(
            keys[0].1.job_id, first.job_ids[0].1,
            "the first opted-out job holds the key"
        );

        // A mergeable ingest of the same cell joins the job the key names, rather
        // than minting a third. That is what makes the opt-out mint-time only.
        let third = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("cell.json".to_string(), cell_body("a", json!({})))],
        )
        .await?;
        assert_eq!(
            third.job_ids[0].1, first.job_ids[0].1,
            "the later plan borrows it"
        );
        assert_eq!(third.merged_job_count, 1);
        assert_eq!(third.merges[0].kind, MergeKind::AcrossPlans);
        assert_eq!(
            stores.todo.list_avail().await?.len(),
            2,
            "two opted-out mints, and no job for the plan that borrowed one"
        );

        // The borrowed job is held by both plans; the second's own job by one.
        assert_eq!(
            members(&stores, &first.job_ids[0].1).await?.len(),
            2,
            "the lending plan and the borrowing plan both hold it"
        );
        assert_eq!(
            members(&stores, &second.job_ids[0].1).await?,
            vec![second.plan_id.clone()]
        );
        Ok(())
    }

    /// The opt-out skips the run-local grouping too: a plan that measures
    /// variance submits its duplicates together.
    #[tokio::test]
    async fn opted_out_siblings_of_one_plan_do_not_share() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let body = cell_body("a", json!({"merge_duplicates": false}));

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                ("one.json".to_string(), body.clone()),
                ("two.json".to_string(), body),
            ],
        )
        .await?;

        assert_ne!(report.job_ids[0].1, report.job_ids[1].1);
        assert_eq!(report.job_count, 2);
        assert_eq!(report.merged_job_count, 0);
        Ok(())
    }

    /// Two identical inputs that both decline the same incumbent share the one
    /// job the pass mints. The decline records its mint, so the sibling joins it
    /// rather than minting a second copy of the cell.
    #[tokio::test]
    async fn declined_siblings_share_the_job_the_pass_mints() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);

        // A record naming a job that is not queued, young enough that the ingest
        // which claimed the digest may still be staging it. Both inputs decline
        // against it and leave it alone.
        let unqueued = JobId::from_uuid(uuid::Uuid::now_v7());
        seed_content_key(&stores, &digest, &unqueued, Utc::now()).await?;

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                ("one.json".to_string(), body.clone()),
                ("two.json".to_string(), body),
            ],
        )
        .await?;

        assert_eq!(report.job_ids[0].1, report.job_ids[1].1, "one job, shared");
        assert_eq!(report.job_count, 1);
        assert_eq!(report.merged_job_count, 1);
        assert_eq!(report.merges[0].kind, MergeKind::WithinPlan);
        assert_eq!(
            stores.todo.list_avail().await?.len(),
            1,
            "the second input mints nothing of its own"
        );
        Ok(())
    }

    /// A sibling of a cross-plan join is `across_plans` too. The cell came from
    /// the earlier plan, whichever input of this plan reached it first — the
    /// second input reads the first's placement, not the index.
    #[tokio::test]
    async fn a_sibling_of_a_cross_plan_join_is_labelled_across_plans() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let body = cell_body("a", json!({}));

        let first = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("cell.json".to_string(), body.clone())],
        )
        .await?;
        let second = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                ("one.json".to_string(), body.clone()),
                ("two.json".to_string(), body),
            ],
        )
        .await?;

        assert!(
            second
                .job_ids
                .iter()
                .all(|(_, id)| *id == first.job_ids[0].1),
            "both inputs land on the earlier plan's job"
        );
        assert_eq!(second.job_count, 1);
        assert_eq!(second.merged_job_count, 2, "both inputs merged");
        assert!(
            second
                .merges
                .iter()
                .all(|m| m.kind == MergeKind::AcrossPlans),
            "the cell came from the earlier plan for both inputs"
        );
        Ok(())
    }

    /// Two inputs of one plan that describe the same cell share one job. Both
    /// labels map to it, the manifest lists it once, and the plan holds one
    /// membership.
    #[tokio::test]
    async fn two_inputs_of_one_plan_share_one_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                ("first.json".to_string(), cell_body("a", json!({}))),
                ("second.json".to_string(), cell_body("a", json!({}))),
                ("other.json".to_string(), cell_body("b", json!({}))),
            ],
        )
        .await?;

        assert_eq!(report.job_count, 2, "three inputs, two distinct cells");
        assert_eq!(report.merged_job_count, 1);
        assert_eq!(report.merges[0].label, "second.json");
        assert_eq!(report.merges[0].kind, MergeKind::WithinPlan);
        assert_eq!(
            report.job_ids.len(),
            3,
            "every input reports the job it landed on"
        );
        assert_eq!(report.job_ids[0].1, report.job_ids[1].1);
        assert_ne!(report.job_ids[0].1, report.job_ids[2].1);
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest written")?;
        assert_eq!(manifest.job_ids.len(), 2);
        assert_eq!(stores.todo.list_avail().await?.len(), 2);
        assert_eq!(
            members(&stores, &report.job_ids[0].1).await?,
            vec![report.plan_id.clone()],
            "one membership per plan per job, however many inputs share it"
        );
        Ok(())
    }

    /// A joiner shares the job whatever its horizon: the queue keeps one entry
    /// under the incumbent's deadline, and the joiner's horizon goes in its
    /// membership marker. `offset_days` of `None` asks for `never`.
    #[rstest]
    #[case::equal(Some(0))]
    #[case::later(Some(150))]
    #[case::never(None)]
    #[case::sooner(Some(-30))]
    #[tokio::test]
    async fn a_joiner_shares_the_job_under_its_own_horizon(
        #[case] offset_days: Option<i64>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let incumbent = in_days(90);
        let want_horizon = match offset_days {
            Some(days) => ExpiresAt::At(incumbent + Duration::days(days)),
            None => ExpiresAt::Never,
        };
        let second_expiry = match want_horizon {
            ExpiresAt::At(at) => rfc3339(at),
            ExpiresAt::Never => "never".to_string(),
        };

        let first = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "a.json".to_string(),
                cell_body("a", json!({"expires_at": rfc3339(incumbent)})),
            )],
        )
        .await?;
        let second = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "a.json".to_string(),
                cell_body("a", json!({"expires_at": second_expiry})),
            )],
        )
        .await?;

        let job_id = &first.job_ids[0].1;
        assert_eq!(&second.job_ids[0].1, job_id, "both plans share one job");
        // The joiner accepts the deadline the key carries rather than moving it,
        // so the queue keeps exactly one entry for the shared job.
        let keys = stores.todo.list_avail().await?;
        assert_eq!(
            keys,
            vec![format!("{job_id}/{}.json", ExpiresAt::At(incumbent))]
        );

        let mut markers = stores.plans.list_job_plan_markers(job_id).await?;
        markers.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let horizons: Vec<(PlanId, ExpiresAt)> = vec![
            (first.plan_id.clone(), ExpiresAt::At(incumbent)),
            (second.plan_id.clone(), want_horizon),
        ];
        let mut want = horizons;
        want.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        assert_eq!(markers, want, "each plan records the horizon it asked for");
        Ok(())
    }

    /// Two inputs of one plan on a job it joined hold it under one marker, at
    /// the latest of their horizons, as for a job the plan minted.
    #[tokio::test]
    async fn a_joined_job_carries_the_latest_horizon_of_its_inputs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let (sooner, incumbent, later) = (in_days(60), in_days(90), in_days(240));
        let first = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "a.json".to_string(),
                cell_body("a", json!({"expires_at": rfc3339(incumbent)})),
            )],
        )
        .await?;
        let second = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                (
                    "one.json".to_string(),
                    cell_body("a", json!({"expires_at": rfc3339(sooner)})),
                ),
                (
                    "two.json".to_string(),
                    cell_body("a", json!({"expires_at": rfc3339(later)})),
                ),
            ],
        )
        .await?;

        let job_id = &first.job_ids[0].1;
        assert_eq!(second.job_count, 1, "both inputs land on the shared job");
        let mut horizons: Vec<ExpiresAt> = stores
            .plans
            .list_job_plan_markers(job_id)
            .await?
            .into_iter()
            .filter(|(plan_id, _)| *plan_id == second.plan_id)
            .map(|(_, expires_at)| expires_at)
            .collect();
        horizons.sort();
        assert_eq!(
            horizons,
            vec![ExpiresAt::At(later)],
            "one marker, at the later of the two inputs' horizons"
        );
        Ok(())
    }

    /// A plan that places several inputs on one job it **mints** holds it under
    /// one marker, at the latest of their horizons — and the job's `avail/` key
    /// carries that same value, so the plan's last question is still answerable.
    #[tokio::test]
    async fn a_minted_job_carries_the_latest_horizon_of_its_inputs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let (sooner, later) = (in_days(60), in_days(240));

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![
                (
                    "one.json".to_string(),
                    cell_body("a", json!({"expires_at": rfc3339(sooner)})),
                ),
                (
                    "two.json".to_string(),
                    cell_body("a", json!({"expires_at": rfc3339(later)})),
                ),
            ],
        )
        .await?;

        let job_id = &report.job_ids[0].1;
        assert_eq!(report.job_count, 1);
        assert_eq!(
            stores.todo.list_avail().await?,
            vec![format!("{job_id}/{}.json", ExpiresAt::At(later))]
        );
        assert_eq!(
            stores.plans.list_job_plan_markers(job_id).await?,
            vec![(report.plan_id.clone(), ExpiresAt::At(later))],
            "one marker per plan per minted job, at the latest horizon"
        );
        // The body's stamped copy follows the key, so the claim path's
        // body-versus-key warning stays silent for an ingested job.
        let body = stores
            .todo
            .get_avail_by_job(job_id)
            .await?
            .context("the minted job is queued")?;
        assert_eq!(body["expires_at"], ExpiresAt::At(later).to_string());
        Ok(())
    }

    /// An `avail/` entry past its deadline is dead, so no plan joins it. Its
    /// content key stands while the record is young, and is taken over once the
    /// record is past the `creating` deadline.
    #[rstest]
    #[case::young_record(false)]
    #[case::old_record(true)]
    #[tokio::test]
    async fn an_expired_entry_is_never_joined(#[case] age_out_record: bool) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        // A job whose deadline lapsed after it was staged, still sitting in
        // `avail/`. Ingestion refuses a lapsed `expires_at`, so the test seeds
        // the entry and its content key directly.
        let first = JobId::from_uuid(uuid::Uuid::now_v7());
        let body = cell_body("a", json!({}));
        let created_at = if age_out_record {
            Utc::now() - Duration::days(1)
        } else {
            Utc::now()
        };
        seed_content_key(&stores, &content_digest(&body), &first, created_at).await?;
        let lapsed = ExpiresAt::At(Utc::now() - Duration::days(1));
        seed_avail(&stores, &first, &body, lapsed).await?;

        let second = ingest(
            &stores,
            &catalog()?,
            None,
            vec![(
                "a.json".to_string(),
                cell_body("a", json!({"expires_at": "never"})),
            )],
        )
        .await?;

        assert_ne!(
            first, second.job_ids[0].1,
            "a lapsed entry is dead, so the second plan mints its own job"
        );
        assert_eq!(second.merged_job_count, 0);
        let owner = stores
            .plans
            .list_content_keys()
            .await?
            .pop()
            .context("the digest still has a record")?
            .1
            .job_id;
        if age_out_record {
            assert_eq!(
                owner, second.job_ids[0].1,
                "past the gate the record is provably an orphan and is taken over"
            );
        } else {
            assert_eq!(owner, first, "inside the gate the record is left alone");
        }
        Ok(())
    }

    /// The join check reads a fresh clock, not the ingest's start time.
    #[tokio::test]
    async fn a_target_that_lapsed_since_ingest_start_is_not_joined() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let target = JobId::from_uuid(uuid::Uuid::now_v7());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);
        seed_content_key(&stores, &digest, &target, Utc::now()).await?;

        // The target sits in `avail/` under a deadline that lapsed half an hour
        // ago — after the ingest below started, before this input's placement.
        let lapsed = ExpiresAt::At(Utc::now() - Duration::minutes(30));
        seed_avail(&stores, &target, &body, lapsed).await?;

        let context = MergeContext {
            plans: stores.plans.as_ref(),
            todo: stores.todo.as_ref(),
            submissions: stores.submissions.as_ref(),
            plan_id: &plan_id,
            leased: tokio::sync::OnceCell::new(),
            // The ingest started an hour ago, when the target was still live.
            now: Utc::now() - Duration::hours(1),
            creating_max_age: CREATING_MAX_AGE,
        };
        let (_, placed) = place_one(&context, &body, ExpiresAt::Never).await?;

        assert!(
            matches!(placed, Placed::Fresh),
            "a lapsed entry is dead, so the plan mints instead of joining"
        );
        assert!(
            members(&stores, &target).await?.is_empty(),
            "no membership marker lands on the lapsed target"
        );
        Ok(())
    }

    /// The record may belong to an ingest still staging, so this plan mints its
    /// own: a crashed ingest costs at most one duplicate.
    #[tokio::test]
    async fn a_record_for_an_unqueued_job_is_left_alone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let unborn = JobId::from_uuid(uuid::Uuid::now_v7());
        let owner = PlanId::from_uuid(uuid::Uuid::now_v7());
        let digest = content_digest(&cell_body("a", json!({})));
        stores
            .plans
            .create_content_key(
                &digest,
                &ContentKeyRecord {
                    job_id: unborn.clone(),
                    plan_id: owner.clone(),
                    created_at: Utc::now(),
                },
            )
            .await?;

        let report = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("a.json".to_string(), cell_body("a", json!({})))],
        )
        .await?;

        assert_eq!(report.merged_job_count, 0);
        assert_ne!(report.job_ids[0].1, unborn);
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].1.job_id, unborn, "the record is untouched");
        assert_eq!(keys[0].1.plan_id, owner);
        Ok(())
    }

    // ── the merge pass's re-check, driven directly ──────────────────────────
    //
    // No ordering of whole ingests opens the window between finding a live
    // candidate and the re-check, so these seed a `leased/` snapshot that the
    // fresh reads disagree with.

    fn merge_context<'a>(
        stores: &'a Stores,
        plan_id: &'a PlanId,
        leased: HashMap<JobId, ExpiresAt>,
    ) -> MergeContext<'a> {
        MergeContext {
            plans: stores.plans.as_ref(),
            todo: stores.todo.as_ref(),
            submissions: stores.submissions.as_ref(),
            plan_id,
            // Pre-seeded: these tests drive the pass with a snapshot that
            // deliberately disagrees with the fresh reads.
            leased: tokio::sync::OnceCell::new_with(Some(leased)),
            now: Utc::now(),
            creating_max_age: CREATING_MAX_AGE,
        }
    }

    /// Queue `body` in `avail/` as `job_id`, stamped as ingestion stamps it, for
    /// a queue state ingestion itself refuses to write.
    async fn seed_avail(
        stores: &Stores,
        job_id: &JobId,
        body: &Value,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let mut queued = body.clone();
        queued["job_id"] = json!(job_id.as_str());
        queued["expires_at"] = json!(expires_at.to_string());
        stores.todo.write_tmp(job_id, &queued).await?;
        stores.todo.promote_avail(job_id, expires_at).await?;
        Ok(())
    }

    async fn seed_content_key(
        stores: &Stores,
        digest: &str,
        job_id: &JobId,
        created_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        stores
            .plans
            .create_content_key(
                digest,
                &ContentKeyRecord {
                    job_id: job_id.clone(),
                    plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
                    created_at,
                },
            )
            .await?;
        Ok(())
    }

    /// Place one input as a unit of its own, under a freshly minted candidate
    /// id. Returns the id it minted, for the tests that assert the digest moved
    /// to it.
    async fn place_one(
        context: &MergeContext<'_>,
        body: &Value,
        plan_expiry: ExpiresAt,
    ) -> anyhow::Result<(JobId, Placed)> {
        let unit = Unit {
            lead: 0,
            horizon: plan_expiry,
            candidate: JobId::from_uuid(uuid::Uuid::now_v7()),
            digest: content_digest(body),
        };
        let placed = context.place(&unit, body, &Inherited::Nothing).await?;
        Ok((unit.candidate, placed))
    }

    /// A candidate that **completes** inside the window is the wanted outcome:
    /// the joining plan reads the same submission record, so the join stands.
    #[tokio::test]
    async fn a_candidate_that_completed_in_the_window_is_still_joined() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let target = JobId::from_uuid(uuid::Uuid::now_v7());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let digest = content_digest(&cell_body("a", json!({})));
        seed_content_key(&stores, &digest, &target, Utc::now()).await?;
        // The job is gone from the queue but its result is recorded.
        stores
            .submissions
            .write_processed(&target, &json!({"job_id": target.as_str()}))
            .await?;

        let context = merge_context(
            &stores,
            &plan_id,
            HashMap::from([(target.clone(), ExpiresAt::Never)]),
        );
        let (_, placed) = place_one(&context, &cell_body("a", json!({})), ExpiresAt::Never).await?;

        assert!(
            matches!(&placed, Placed::Join(t) if t.job_id == target),
            "a completed candidate is still the job this plan wants"
        );
        assert_eq!(members(&stores, &target).await?, vec![plan_id]);
        Ok(())
    }

    /// A candidate that **vanishes** inside the window leaves the plan nothing to
    /// wait on, so the pass mints instead, drops the marker it wrote, and takes
    /// the digest over.
    #[tokio::test]
    async fn a_candidate_that_vanished_in_the_window_yields_a_fresh_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let target = JobId::from_uuid(uuid::Uuid::now_v7());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let digest = content_digest(&cell_body("a", json!({})));
        seed_content_key(&stores, &digest, &target, Utc::now()).await?;

        let context = merge_context(
            &stores,
            &plan_id,
            HashMap::from([(target.clone(), ExpiresAt::Never)]),
        );
        let (candidate, placed) =
            place_one(&context, &cell_body("a", json!({})), ExpiresAt::Never).await?;

        assert!(matches!(placed, Placed::Fresh));
        assert!(
            members(&stores, &target).await?.is_empty(),
            "the marker written for the vanished candidate is dropped again"
        );
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(keys[0].1.job_id, candidate, "the digest is taken over");
        Ok(())
    }

    /// A claim renames `avail/` into `leased/`, so a job claimed after the
    /// snapshot is in neither. It is live, so the pass joins it rather than
    /// taking over its aged record, which a claim never touches.
    #[tokio::test]
    async fn a_candidate_claimed_after_the_snapshot_is_still_joined() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let target = JobId::from_uuid(uuid::Uuid::now_v7());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);
        seed_content_key(&stores, &digest, &target, Utc::now() - Duration::days(1)).await?;

        // Queue the target, then claim it, so it sits in `leased/` alone.
        stores.todo.write_tmp(&target, &body).await?;
        stores.todo.promote_avail(&target, ExpiresAt::Never).await?;
        let claimed = stores
            .todo
            .claim_job(
                &target,
                ExpiresAt::Never,
                &ClientId::try_new("ev1_a")?,
                Utc::now() + Duration::hours(1),
            )
            .await?;
        assert!(
            matches!(claimed, ClaimResult::Claimed(_)),
            "the target is claimed before the pass runs"
        );

        // An empty snapshot is what an ingest holds when the claim lands after
        // its own listing.
        let context = merge_context(&stores, &plan_id, HashMap::new());
        let (_, placed) = place_one(&context, &body, ExpiresAt::Never).await?;

        assert!(
            matches!(&placed, Placed::Join(t) if t.job_id == target),
            "a claimed candidate is live, so this plan joins it"
        );
        assert_eq!(members(&stores, &target).await?, vec![plan_id]);
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(
            keys[0].1.job_id, target,
            "the running job keeps its content key"
        );
        Ok(())
    }

    /// Queued under an unparseable deadline, so the pass mints alongside it:
    /// reading it as absent would let [`MergeContext::replace_stale`] take the
    /// digest over from a queued job.
    #[tokio::test]
    async fn a_candidate_whose_deadline_is_unreadable_is_not_taken_over() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let target = JobId::from_uuid(uuid::Uuid::now_v7());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);
        // Past the age gate, which is the state of every job an earlier plan
        // queued.
        seed_content_key(&stores, &digest, &target, Utc::now() - Duration::days(1)).await?;

        // Queued under a name whose deadline does not parse. Written straight
        // into `avail/`, because every name the store itself builds parses.
        let mut queued = body.clone();
        queued["job_id"] = json!(target.as_str());
        let job_dir = dir.path().join("todo").join("avail").join(target.as_str());
        std::fs::create_dir_all(&job_dir)?;
        std::fs::write(
            job_dir.join("not-a-deadline.json"),
            serde_json::to_vec(&queued)?,
        )?;

        let context = merge_context(&stores, &plan_id, HashMap::new());
        let (_, placed) = place_one(&context, &body, ExpiresAt::Never).await?;

        assert!(
            matches!(placed, Placed::Fresh),
            "an unreadable deadline declines the merge rather than joining blind"
        );
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(
            keys[0].1.job_id, target,
            "the queued job keeps its content key"
        );
        assert!(
            members(&stores, &target).await?.is_empty(),
            "the pass declines before it writes a membership marker"
        );
        Ok(())
    }

    /// A [`TodoStore`] that recycles one lease at the first `leased/` listing,
    /// between the pass's first `avail/` read and its `leased/` read. Any other
    /// method lands in `unexpected`, which the test asserts empty.
    struct RecyclesAtFirstLeasedListing {
        inner: std::sync::Arc<dyn TodoStore>,
        unexpected: pipette_store_testkit::UnexpectedCalls,
        listings: std::sync::atomic::AtomicUsize,
        job_id: JobId,
        expires_at: ExpiresAt,
        client_id: ClientId,
        lease_expiry: DateTime<Utc>,
    }

    #[async_trait::async_trait]
    impl TodoStore for RecyclesAtFirstLeasedListing {
        async fn avail_key_for_job(&self, job_id: &JobId) -> anyhow::Result<Option<String>> {
            self.inner.avail_key_for_job(job_id).await
        }
        async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
            if self
                .listings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                self.inner
                    .recycle_lease(
                        &self.job_id,
                        self.expires_at,
                        &self.client_id,
                        self.lease_expiry,
                    )
                    .await?;
            }
            self.inner.list_leased().await
        }
        async fn list_avail(&self) -> anyhow::Result<Vec<String>> {
            Err(self.unexpected.record("list_avail"))
        }
        async fn get_avail(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
        ) -> anyhow::Result<Option<Value>> {
            Err(self.unexpected.record("get_avail"))
        }
        async fn delete_avail(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_avail"))
        }
        async fn delete_avail_by_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_avail_by_job"))
        }
        async fn get_avail_by_job(&self, _job_id: &JobId) -> anyhow::Result<Option<Value>> {
            Err(self.unexpected.record("get_avail_by_job"))
        }
        async fn redate_avail(
            &self,
            _job_id: &JobId,
            _from: ExpiresAt,
            _to: ExpiresAt,
        ) -> anyhow::Result<pipette_store::RedateResult> {
            Err(self.unexpected.record("redate_avail"))
        }
        async fn list_leased_for_client(
            &self,
            _client_id: &ClientId,
        ) -> anyhow::Result<Vec<String>> {
            Err(self.unexpected.record("list_leased_for_client"))
        }
        async fn get_leased(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
            _client_id: &ClientId,
            _lease_expiry: DateTime<Utc>,
        ) -> anyhow::Result<Option<Value>> {
            Err(self.unexpected.record("get_leased"))
        }
        async fn renew_lease(
            &self,
            _job_id: &JobId,
            _client_id: &ClientId,
            _new_expiry: DateTime<Utc>,
        ) -> anyhow::Result<pipette_store::RenewLeaseResult> {
            Err(self.unexpected.record("renew_lease"))
        }
        async fn delete_lease(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
            _client_id: &ClientId,
            _expiry: DateTime<Utc>,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_lease"))
        }
        async fn claim_job(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
            _client_id: &ClientId,
            _lease_expiry: DateTime<Utc>,
        ) -> anyhow::Result<ClaimResult> {
            Err(self.unexpected.record("claim_job"))
        }
        async fn recycle_lease(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
            _client_id: &ClientId,
            _lease_expiry: DateTime<Utc>,
        ) -> anyhow::Result<pipette_store::RecycleResult> {
            Err(self.unexpected.record("recycle_lease"))
        }
        async fn write_denied(
            &self,
            _job_id: &JobId,
            _client_id: &ClientId,
            _record: &DenialRecord,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_denied"))
        }
        async fn get_denied(
            &self,
            _job_id: &JobId,
            _client_id: &ClientId,
        ) -> anyhow::Result<Option<DenialRecord>> {
            Err(self.unexpected.record("get_denied"))
        }
        async fn is_denied(&self, _job_id: &JobId, _client_id: &ClientId) -> anyhow::Result<bool> {
            Err(self.unexpected.record("is_denied"))
        }
        async fn delete_denied_for_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_denied_for_job"))
        }
        async fn delete_denied(
            &self,
            _job_id: &JobId,
            _client_id: &ClientId,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_denied"))
        }
        async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
            Err(self.unexpected.record("list_all_denied"))
        }
        async fn list_eligible_for_client(
            &self,
            _client_id: &ClientId,
        ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
            Err(self.unexpected.record("list_eligible_for_client"))
        }
        async fn write_eligible(
            &self,
            _client_id: &ClientId,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_eligible"))
        }
        async fn delete_eligible_for_client(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_eligible_for_client"))
        }
        async fn delete_eligible(
            &self,
            _client_id: &ClientId,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_eligible"))
        }
        async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
            Err(self.unexpected.record("list_all_eligible"))
        }
        async fn write_pending_reindex(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_pending_reindex"))
        }
        async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
            Err(self.unexpected.record("list_pending_reindex"))
        }
        async fn delete_pending_reindex(&self, _key: &str) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_pending_reindex"))
        }
        async fn has_pending_reindex(&self, _client_id: &ClientId) -> anyhow::Result<bool> {
            Err(self.unexpected.record("has_pending_reindex"))
        }
        async fn write_pending_reindex_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_pending_reindex_job"))
        }
        async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>> {
            Err(self.unexpected.record("list_pending_reindex_jobs"))
        }
        async fn delete_pending_reindex_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_pending_reindex_job"))
        }
        async fn write_lost_lease(
            &self,
            _job_id: &JobId,
            _client_id: &ClientId,
            _lease_expiry: DateTime<Utc>,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_lost_lease"))
        }
        async fn list_lost_leases(
            &self,
            _job_id: &JobId,
        ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
            Err(self.unexpected.record("list_lost_leases"))
        }
        async fn delete_lost_leases_for_job(&self, _job_id: &JobId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_lost_leases_for_job"))
        }
        async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>> {
            Err(self.unexpected.record("list_lost_lease_jobs"))
        }
        async fn write_tmp(&self, _job_id: &JobId, _body: &Value) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_tmp"))
        }
        async fn promote_avail(
            &self,
            _job_id: &JobId,
            _expires_at: ExpiresAt,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("promote_avail"))
        }
        async fn list_stale_tmp(&self, _age: std::time::Duration) -> anyhow::Result<Vec<String>> {
            Err(self.unexpected.record("list_stale_tmp"))
        }
        async fn delete_tmp_object(&self, _key: &str) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_tmp_object"))
        }
        async fn read_eligible_indexed(&self) -> anyhow::Result<HashSet<String>> {
            Err(self.unexpected.record("read_eligible_indexed"))
        }
        async fn write_eligible_indexed(&self, _indexed: &HashSet<String>) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_eligible_indexed"))
        }
        async fn read_eligible_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
            Err(self.unexpected.record("read_eligible_workers"))
        }
        async fn write_eligible_workers(&self, _workers: &HashSet<ClientId>) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_eligible_workers"))
        }
        async fn read_backlog(&self) -> anyhow::Result<Option<pipette_store::BacklogSnapshot>> {
            Err(self.unexpected.record("read_backlog"))
        }
        async fn write_backlog(
            &self,
            _snapshot: &pipette_store::BacklogSnapshot,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_backlog"))
        }
        async fn read_campaign_backlog(
            &self,
        ) -> anyhow::Result<Option<pipette_store::CampaignBacklogSnapshot>> {
            Err(self.unexpected.record("read_campaign_backlog"))
        }
        async fn write_campaign_backlog(
            &self,
            _snapshot: &pipette_store::CampaignBacklogSnapshot,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_campaign_backlog"))
        }
        async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
            Err(self.unexpected.record("read_gc_candidates"))
        }
        async fn write_gc_candidates(&self, _candidates: &HashSet<String>) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_gc_candidates"))
        }
        async fn write_suspension(
            &self,
            _client_id: &ClientId,
            _suspended_at: DateTime<Utc>,
            _conflicting_job_id: &JobId,
        ) -> anyhow::Result<()> {
            Err(self.unexpected.record("write_suspension"))
        }
        async fn read_suspension(
            &self,
            _client_id: &ClientId,
        ) -> anyhow::Result<Option<pipette_store::SuspensionRecord>> {
            Err(self.unexpected.record("read_suspension"))
        }
        async fn delete_suspension(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_suspension"))
        }
        async fn list_suspensions(
            &self,
        ) -> anyhow::Result<Vec<(ClientId, pipette_store::SuspensionRecord)>> {
            Err(self.unexpected.record("list_suspensions"))
        }
        async fn list_suspended_ids(&self) -> anyhow::Result<Vec<ClientId>> {
            Err(self.unexpected.record("list_suspended_ids"))
        }
        async fn stamp_liveness(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("stamp_liveness"))
        }
        async fn read_liveness(
            &self,
            _client_id: &ClientId,
        ) -> anyhow::Result<Option<DateTime<Utc>>> {
            Err(self.unexpected.record("read_liveness"))
        }
        async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
            Err(self.unexpected.record("list_liveness"))
        }
        async fn delete_liveness(&self, _client_id: &ClientId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_liveness"))
        }
        async fn validate_backend(&self) -> anyhow::Result<()> {
            Err(self.unexpected.record("validate_backend"))
        }
    }

    /// Without the pass's second `avail/` read, a job recycled back between the
    /// queue reads would read as gone and lose its record while it runs on.
    #[tokio::test]
    async fn a_candidate_recycled_between_the_queue_reads_is_still_joined() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let target = JobId::from_uuid(uuid::Uuid::now_v7());
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let holder = ClientId::try_new("ev1_a")?;
        let lease_expiry = Utc::now() + Duration::hours(1);
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);
        seed_content_key(&stores, &digest, &target, Utc::now() - Duration::days(1)).await?;

        // Queue the target with a readable deadline, then claim it, so it starts
        // in `leased/` alone.
        seed_avail(&stores, &target, &body, ExpiresAt::Never).await?;
        assert!(
            matches!(
                stores
                    .todo
                    .claim_job(&target, ExpiresAt::Never, &holder, lease_expiry)
                    .await?,
                ClaimResult::Claimed(_)
            ),
            "the target is claimed before the pass runs"
        );

        let recycling = RecyclesAtFirstLeasedListing {
            inner: stores.todo.clone(),
            unexpected: pipette_store_testkit::UnexpectedCalls::default(),
            listings: std::sync::atomic::AtomicUsize::new(0),
            job_id: target.clone(),
            expires_at: ExpiresAt::Never,
            client_id: holder.clone(),
            lease_expiry,
        };
        let context = MergeContext {
            plans: stores.plans.as_ref(),
            todo: &recycling,
            submissions: stores.submissions.as_ref(),
            plan_id: &plan_id,
            // Pre-seeded empty, as an ingest holds when the claim lands after
            // its listing.
            leased: tokio::sync::OnceCell::new_with(Some(HashMap::new())),
            now: Utc::now(),
            creating_max_age: CREATING_MAX_AGE,
        };
        let (_, placed) = place_one(&context, &body, ExpiresAt::Never).await?;
        recycling.unexpected.none()?;

        assert!(
            matches!(&placed, Placed::Join(t) if t.job_id == target),
            "a recycled candidate is live, so this plan joins it"
        );
        assert_eq!(members(&stores, &target).await?, vec![plan_id]);
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(
            keys[0].1.job_id, target,
            "the recycled job keeps its content key"
        );
        Ok(())
    }

    /// A concurrent ingest of one cell: both scan, neither sees a record, and the
    /// loser's exclusive create is refused. It joins the winner's job rather than
    /// minting a duplicate.
    #[tokio::test]
    async fn a_lost_create_joins_the_winners_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);

        // The winner's job, queued and indexed.
        let winner = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("a.json".to_string(), body.clone())],
        )
        .await?;

        // The loser reaches the claim with a record already in place.
        let context = merge_context(&stores, &plan_id, HashMap::new());
        let placed = context
            .claim(
                &digest,
                &JobId::from_uuid(uuid::Uuid::now_v7()),
                ExpiresAt::Never,
            )
            .await?;

        assert!(
            matches!(&placed, Placed::Join(t)
                if t.job_id == winner.job_ids[0].1 && t.kind == MergeKind::AcrossPlans),
            "the loser of the create joins the winner"
        );
        Ok(())
    }

    /// A lost compare-and-swap on the content key is re-read once, and the join
    /// is retried against whatever now holds the digest. The record the swap
    /// refused to overwrite is never destroyed.
    #[tokio::test]
    async fn a_lost_swap_retries_the_join_against_the_new_incumbent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let body = cell_body("a", json!({}));
        let digest = content_digest(&body);

        let incumbent = ingest(
            &stores,
            &catalog()?,
            None,
            vec![("a.json".to_string(), body.clone())],
        )
        .await?;

        // A version no record ever had: the swap is refused, exactly as it is
        // when a rival replaced the record since this pass read it.
        let stale_version = ContentKeyVersion::Digest("not-the-current-bytes".to_string());
        let context = merge_context(&stores, &plan_id, HashMap::new());
        let placed = context
            .replace_stale(
                &digest,
                &stale_version,
                &JobId::from_uuid(uuid::Uuid::now_v7()),
                ExpiresAt::Never,
            )
            .await?;

        assert!(
            matches!(&placed, Placed::Join(t) if t.job_id == incumbent.job_ids[0].1),
            "the retry joins the job that now holds the digest"
        );
        let keys = stores.plans.list_content_keys().await?;
        assert_eq!(
            keys[0].1.job_id, incumbent.job_ids[0].1,
            "the refused swap left the incumbent's record intact"
        );
        Ok(())
    }

    // ── model parameter table coverage ──────────────────────────────────────

    /// A table holding one model, so anything else is uncatalogued.
    fn model_table() -> anyhow::Result<ModelParamTable> {
        ModelParamTable::from_toml(r#""LFM2-700M" = 742"#)
    }

    /// A `gguf_text` run specification naming the Hugging Face `repo`.
    /// `runtime.version` varies the cell without touching eligibility or the
    /// model, so two jobs on one model stay distinct cells.
    fn spec_with_model(repo: &str, runtime_version: &str) -> Value {
        json!({
            "benchmark": "bench-1",
            "model": {"type": "gguf_text", "source": "huggingface", "org": "org", "repo_name": repo, "path": "model.gguf"},
            "runtime": {"version": runtime_version},
        })
    }

    fn model_job(label: &str, repo: &str) -> (String, Value) {
        model_job_v(label, repo, "v1")
    }

    fn model_job_v(label: &str, repo: &str, runtime_version: &str) -> (String, Value) {
        (
            label.to_string(),
            json!({
                "requires": ["os:macos"],
                "spec": spec_with_model(repo, runtime_version),
            }),
        )
    }

    /// The jobs still queue and the manifest is `active`; the report names the
    /// model.
    #[tokio::test]
    async fn an_uncatalogued_model_warns_without_blocking_the_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;
        let models = model_table()?;

        let report = ingest_with_models(
            &stores,
            &catalog()?,
            ModelCheck::warn(&models),
            None,
            None,
            vec![model_job("a.json", "K2-Horizon-3.7B-GGUF")],
        )
        .await?;

        assert_eq!(report.job_count, 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0]
                .message
                .contains("no parameter-count entry")
        );
        assert_eq!(
            report.warnings[0].job_ids,
            vec![report.job_ids[0].1.clone()]
        );

        // The job is queued and the manifest carries the warning frozen, like
        // any other ingestion-time warning.
        assert!(
            stores
                .todo
                .get_avail_by_job(&report.job_ids[0].1)
                .await?
                .is_some()
        );
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest written")?;
        assert_eq!(manifest.status, PlanStatus::Active);
        assert_eq!(manifest.warnings.len(), 1);
        Ok(())
    }

    /// A known model draws no warning and passes strict mode, named with or
    /// without its `-GGUF` suffix. The mode is a `bool` because the check
    /// borrows a table that is built inside the body.
    #[rstest]
    #[case::warn_bare_repo(false, "LFM2-700M-GGUF")]
    #[case::strict_bare_repo(true, "LFM2-700M")]
    #[tokio::test]
    async fn a_catalogued_model_passes(
        #[case] strict: bool,
        #[case] repo: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;
        let models = model_table()?;
        let check = if strict {
            ModelCheck::strict(&models)
        } else {
            ModelCheck::warn(&models)
        };

        let report = ingest_with_models(
            &stores,
            &catalog()?,
            check,
            None,
            None,
            vec![model_job("a.json", repo)],
        )
        .await?;

        assert_eq!(report.job_count, 1);
        assert!(report.warnings.is_empty());
        Ok(())
    }

    /// Jobs are grouped by the model they name, so a plan sweeping one model
    /// across a fleet reports one warning rather than one per job. Two models
    /// make two warnings, each naming only its own jobs.
    #[tokio::test]
    async fn warnings_group_by_model() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;
        let models = model_table()?;

        let report = ingest_with_models(
            &stores,
            &catalog()?,
            ModelCheck::warn(&models),
            None,
            None,
            vec![
                model_job_v("a.json", "K2-Horizon-3.7B-GGUF", "v1"),
                model_job_v("b.json", "MiniCPM5-2B-GGUF", "v1"),
                model_job_v("c.json", "K2-Horizon-3.7B-GGUF", "v2"),
            ],
        )
        .await?;

        // Three inputs, three distinct cells (the two K2-Horizon jobs differ by
        // runtime version), two warnings.
        assert_eq!(report.job_count, 3);
        assert_eq!(report.warnings.len(), 2);
        let counts: Vec<usize> = report.warnings.iter().map(|w| w.job_ids.len()).collect();
        assert_eq!(counts, vec![2, 1]);
        Ok(())
    }

    /// The refusal names the model, so an operator knows what to add.
    #[tokio::test]
    async fn strict_rejects_an_uncatalogued_model_before_writing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;
        let models = model_table()?;

        let err = ingest_with_models(
            &stores,
            &catalog()?,
            ModelCheck::strict(&models),
            None,
            None,
            vec![model_job("a.json", "K2-Horizon-3.7B-GGUF")],
        )
        .await
        .err()
        .context("strict mode must reject an uncatalogued model")?;

        assert!(matches!(
            err,
            IngestError::Rejected(RejectionError::Job {
                kind: JobRejectionError::UncataloguedModel { .. },
                ..
            })
        ));
        // Fail-fast: validation runs before any write, so the store is untouched.
        assert!(
            pipette_store::drain_plans(stores.plans.as_ref(), None)
                .await?
                .items
                .is_empty()
        );
        Ok(())
    }

    /// A gated model carries its repo token in `spec.model`, so every place
    /// this check names a model has to name it the way the warehouse stores a
    /// descriptor: with every `auth_token` dropped.
    fn gated_model_job() -> (String, Value) {
        (
            "a.json".to_string(),
            json!({
                "requires": ["os:macos"],
                "spec": {
                    "benchmark": "bench-1",
                    "model": {
                        "type": "gguf_text",
                        "source": "huggingface",
                        "org": "gated",
                        "repo_name": "K2-Horizon-3.7B-GGUF",
                        "path": "m.gguf",
                        "auth_token": pipette_hub_testkit::GOOD_TOKEN,
                    },
                    "runtime": {"version": "v1"},
                },
            }),
        )
    }

    /// The warning is frozen onto the manifest, so a token in it is a token
    /// published. It names the HuggingFace model that the import replaced.
    #[tokio::test]
    async fn an_uncatalogued_gated_model_warns_without_its_token() -> anyhow::Result<()> {
        use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo};

        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;
        let models = model_table()?;
        let hub = MockHub::start(HashMap::from([(
            "gated/K2-Horizon-3.7B-GGUF".to_string(),
            MockRepo {
                commit: "e".repeat(40),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![MockFile::lfs("m.gguf", b"GGUF weights")],
            },
        )]))
        .await?;
        let imports = HfImports::new(
            hub.endpoint.as_str(),
            NonZeroUsize::MIN,
            NonZeroUsize::MIN.saturating_add(1),
            std::num::NonZeroU64::MAX,
        )?;
        let filer = <UserId as pipette_core::types::Mint>::mint();

        let report = ingest_jobs(
            &IngestContext {
                import: Some(&imports),
                ..IngestContext::from_stores(
                    &stores,
                    &catalog()?,
                    ModelCheck::warn(&models),
                    MAX_JOBS,
                    CREATING_MAX_AGE,
                )
            },
            PlanSubmission {
                note: None,
                change: None,
                submitted_by: Some(filer.clone()),
                membership: CampaignMembership::for_test(filer),
                jobs: vec![gated_model_job()],
                source: None,
                plan_warnings: Vec::new(),
            },
        )
        .await?;

        let [warning] = report.warnings.as_slice() else {
            anyhow::bail!("not one warning: {:?}", report.warnings);
        };
        assert!(
            warning.message.contains("\"source\":\"huggingface\""),
            "the warning names the HuggingFace model: {}",
            warning.message
        );
        let manifest = stores
            .plans
            .get_plan(&report.plan_id)
            .await?
            .context("manifest written")?;
        let (_, job_id) = report.job_ids.first().context("one job")?;
        let source = stores
            .plans
            .get_job_source(job_id)
            .await?
            .context("job source written")?;
        assert_eq!(source["spec"]["model"]["source"], "vault_url", "{source}");
        let source = source.to_string();
        let leaked = [&warning.message, &manifest.warnings[0].message, &source]
            .into_iter()
            .find(|text| text.contains("SECRET"));
        assert!(leaked.is_none(), "token leaked into {leaked:?}");
        Ok(())
    }

    /// The strict refusal names the model too, and that message reaches the
    /// caller and the log.
    #[tokio::test]
    async fn a_strict_refusal_omits_a_gated_models_token() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;
        let models = model_table()?;

        let err = ingest_with_models(
            &stores,
            &catalog()?,
            ModelCheck::strict(&models),
            None,
            None,
            vec![gated_model_job()],
        )
        .await
        .err()
        .context("strict mode must reject an uncatalogued model")?;

        assert!(
            !format!("{err}").contains("SECRET"),
            "token leaked into the rejection: {err}"
        );
        Ok(())
    }

    /// A deployment with no table, or an empty one, checks nothing: every
    /// model resolves, in strict mode too. Reporting every job as uncatalogued
    /// there would be noise, and refusing every plan would be an outage.
    #[rstest]
    #[case::no_catalog(None)]
    #[case::empty_catalog(Some(""))]
    #[tokio::test]
    async fn an_absent_catalog_checks_nothing(#[case] body: Option<&str>) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        enlist(&stores, &client("ev1_a", "macOS", ClientStatus::Approved)?).await?;

        let empty = body.map(ModelParamTable::from_toml).transpose()?;
        let models = empty
            .as_ref()
            .map_or_else(ModelCheck::none, ModelCheck::strict);
        let report = ingest_with_models(
            &stores,
            &catalog()?,
            models,
            None,
            None,
            vec![model_job("a.json", "Nobody-Catalogued-This")],
        )
        .await?;

        assert_eq!(report.job_count, 1);
        assert!(report.warnings.is_empty());
        Ok(())
    }

    // ── satisfying a cell from an earlier run (PIP-541) ─────────────────────

    /// Stores that keep their records in a scratch PostgreSQL database, and
    /// the test [`catalog`]. Only this backend keeps the job-cell index.
    struct SqlEnv {
        stores: Stores,
        catalog: HashMap<BenchmarkId, Benchmark>,
        _db: pipette_store_sql::scratch::TestDb,
        _dir: tempfile::TempDir,
    }

    impl SqlEnv {
        /// Finish `job_id` with a successful run.
        async fn finish(&self, job_id: &JobId) -> anyhow::Result<()> {
            let stores = &self.stores;
            finish_job_with_success(&*stores.submissions, &*stores.todo, job_id, "prefill").await
        }
    }

    /// A fresh [`SqlEnv`]. `None` skips the test.
    async fn sql_env() -> anyhow::Result<Option<SqlEnv>> {
        let Some(db) = pipette_store_sql::scratch::TestDb::new().await? else {
            return Ok(None);
        };
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_sql::overlay(db.db(), stores_in(dir.path())?);
        Ok(Some(SqlEnv {
            stores,
            catalog: catalog()?,
            _db: db,
            _dir: dir,
        }))
    }

    /// [`cell_body`] filed through the roster alone: no `requires`.
    fn roster_body(cell: &str) -> Value {
        let mut body = cell_body(cell, json!({"clients": ["ev1_test"]}));
        if let Some(fields) = body.as_object_mut() {
            fields.remove("requires");
        }
        body
    }

    /// The merge kind of each merged input of `report`, in order.
    fn merge_kinds(report: &IngestReport) -> Vec<MergeKind> {
        report.merges.iter().map(|m| m.kind).collect()
    }

    /// The single job of a report that holds one.
    fn only_job(report: &IngestReport) -> anyhow::Result<JobId> {
        match report.job_ids.as_slice() {
            [(_, job_id)] => Ok(job_id.clone()),
            jobs => anyhow::bail!("one job, got {jobs:?}"),
        }
    }

    /// A cell that an earlier finished run answers mints nothing: the plan
    /// lists that job as satisfied, queues nothing and holds no marker on it.
    #[tokio::test]
    async fn a_cell_with_an_answering_run_mints_nothing() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let catalog = &env.catalog;
        let first = ingest(stores, catalog, None, cells(&["a"], json!({}))).await?;
        let done = only_job(&first)?;
        env.finish(&done).await?;

        let second = ingest(stores, catalog, None, cells(&["a"], json!({}))).await?;

        assert_eq!(only_job(&second)?, done);
        assert_eq!(
            second.merges,
            vec![MergedInput {
                label: "a.json".to_string(),
                job_id: done.clone(),
                kind: MergeKind::Satisfied,
            }]
        );
        assert_eq!((second.job_count, second.merged_job_count), (1, 1));
        assert!(stores.todo.list_avail().await?.is_empty());
        assert!(!members(stores, &done).await?.contains(&second.plan_id));
        let manifest = stores
            .plans
            .get_plan(&second.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.job_ids, vec![done]);
        assert!(manifest.warnings.is_empty(), "{:?}", manifest.warnings);
        Ok(())
    }

    /// The same plan, filed again once its jobs finish, satisfies every cell,
    /// and the plan latches `complete` with every job finished.
    #[tokio::test]
    async fn a_plan_filed_again_after_its_runs_satisfies_every_cell() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let catalog = &env.catalog;
        let first = ingest(stores, catalog, None, cells(&["a", "b"], json!({}))).await?;
        futures::future::try_join_all(first.job_ids.iter().map(|(_, job_id)| env.finish(job_id)))
            .await?;

        let again = ingest(stores, catalog, None, cells(&["a", "b"], json!({}))).await?;

        assert_eq!(again.job_ids, first.job_ids);
        assert!(
            again.merges.iter().all(|m| m.kind == MergeKind::Satisfied),
            "{:?}",
            again.merges
        );
        assert_eq!((again.job_count, again.merged_job_count), (2, 2));
        crate::queue_maintenance::run(
            stores.todo.as_ref(),
            stores.clients.as_ref(),
            stores.tags.as_ref(),
            stores.submissions.as_ref(),
            stores.plans.as_ref(),
            catalog,
            crate::queue_maintenance::MaintenanceSettings {
                tmp_max: CREATING_MAX_AGE,
                plan_creating_max: CREATING_MAX_AGE,
                lost_lease_limit: 3,
            },
        )
        .await?;
        let manifest = stores
            .plans
            .get_plan(&again.plan_id)
            .await?
            .context("manifest")?;
        assert_eq!(manifest.status, PlanStatus::Complete);
        let counts = manifest.progress_snapshot.context("a snapshot")?.counts;
        assert_eq!((counts.finished, counts.total), (2, 2));
        Ok(())
    }

    /// How the earlier job of a cell ended.
    #[derive(Debug, Clone, Copy)]
    enum Ended {
        Success,
        Failure,
    }

    /// The earlier run satisfies a later cell only if all of these hold:
    ///
    /// - The run finished.
    /// - The later input merges.
    /// - The recorded device meets the later clauses
    ///   ([`matching::run_answers`]).
    ///
    /// The recorded device is an Apple M5 MacBook Pro on macOS.
    #[rstest]
    #[case::same_cell(cell_body("a", json!({})), Ended::Success, cell_body("a", json!({})), true)]
    #[case::failed_run(cell_body("a", json!({})), Ended::Failure, cell_body("a", json!({})), false)]
    #[case::opted_out(cell_body("a", json!({})), Ended::Success, cell_body("a", json!({"merge_duplicates": false})), false)]
    #[case::roster_run_on_the_os(roster_body("a"), Ended::Success, cell_body("a", json!({})), true)]
    #[case::roster_run_on_another_os(roster_body("a"), Ended::Success, cell_body("a", json!({"requires": ["os:ios"]})), false)]
    #[case::new_exclusion(cell_body("a", json!({})), Ended::Success, cell_body("a", json!({"excludes": ["chip:a15"]})), false)]
    #[tokio::test]
    async fn an_earlier_run_satisfies_a_cell_it_answers(
        #[case] earlier: Value,
        #[case] ended: Ended,
        #[case] later: Value,
        #[case] satisfied: bool,
    ) -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let catalog = &env.catalog;
        let first = ingest(stores, catalog, None, vec![("a.json".into(), earlier)]).await?;
        let done = only_job(&first)?;
        match ended {
            Ended::Success => env.finish(&done).await?,
            Ended::Failure => {
                finish_job_with_failure(&*stores.submissions, &*stores.todo, &done).await?
            }
        }

        let second = ingest(stores, catalog, None, vec![("a.json".into(), later)]).await?;

        let kinds = merge_kinds(&second);
        if satisfied {
            assert_eq!(only_job(&second)?, done);
            assert_eq!(kinds, vec![MergeKind::Satisfied]);
        } else {
            assert_ne!(only_job(&second)?, done);
            assert!(!kinds.contains(&MergeKind::Satisfied), "{kinds:?}");
            assert_eq!(stores.todo.list_avail().await?.len(), 1);
        }
        Ok(())
    }

    /// A campaign owner by name: `org_*` is an organization, anything else a
    /// user.
    fn owner(name: &str) -> anyhow::Result<VaultOwner> {
        Ok(if name.starts_with(pipette_plan_types::OrgId::PREFIX) {
            VaultOwner::Org {
                org_id: pipette_plan_types::OrgId::try_new(name)?,
            }
        } else {
            VaultOwner::User {
                user_id: UserId::try_new(name)?,
            }
        })
    }

    /// [`ingest`] into a campaign that `owner` owns.
    async fn ingest_owned(
        env: &SqlEnv,
        owner: VaultOwner,
        jobs: Vec<(String, Value)>,
    ) -> anyhow::Result<IngestReport> {
        Ok(ingest_jobs(
            &IngestContext::from_stores(
                &env.stores,
                &env.catalog,
                ModelCheck::none(),
                MAX_JOBS,
                CREATING_MAX_AGE,
            ),
            PlanSubmission {
                note: None,
                change: None,
                submitted_by: None,
                membership: CampaignMembership {
                    owner,
                    ..CampaignMembership::for_test(UserId::try_new("user_test")?)
                },
                jobs,
                source: None,
                plan_warnings: Vec::new(),
            },
        )
        .await?)
    }

    /// An earlier run answers a later campaign only within the scope of its
    /// owner: the organization of the owner, or a user in no organization
    /// alone. `user_a` and `user_b` belong to `org_x`, and `user_c` to `org_y`.
    #[rstest]
    #[case::same_org_other_user("user_a", "user_b", true)]
    #[case::org_and_its_member("org_x", "user_b", true)]
    #[case::member_and_its_org("user_a", "org_x", true)]
    #[case::other_org("user_a", "user_c", false)]
    #[case::other_org_owner("org_y", "user_a", false)]
    #[case::same_user_in_no_org("user_d", "user_d", true)]
    #[case::other_user_in_no_org("user_d", "user_e", false)]
    #[case::member_and_user_in_no_org("user_a", "user_d", false)]
    #[tokio::test]
    async fn an_earlier_run_answers_only_within_its_scope(
        #[case] earlier: &str,
        #[case] later: &str,
        #[case] satisfied: bool,
    ) -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        for (user, org) in [
            ("user_a", "org_x"),
            ("user_b", "org_x"),
            ("user_c", "org_y"),
        ] {
            stores
                .orgs
                .put_membership(&pipette_core::organization::Membership {
                    user_id: UserId::try_new(user)?,
                    org_id: pipette_plan_types::OrgId::try_new(org)?,
                })
                .await?;
        }
        let first = ingest_owned(&env, owner(earlier)?, cells(&["a"], json!({}))).await?;
        let done = only_job(&first)?;
        env.finish(&done).await?;

        let second = ingest_owned(&env, owner(later)?, cells(&["a"], json!({}))).await?;

        assert_eq!(only_job(&second)? == done, satisfied);
        let kinds = merge_kinds(&second);
        let expected = if satisfied {
            vec![MergeKind::Satisfied]
        } else {
            Vec::new()
        };
        assert_eq!(kinds, expected);
        Ok(())
    }

    /// A recalculation mints, even when an earlier finished run of another
    /// plan answers its cell.
    #[tokio::test]
    async fn a_recalculation_mints_beside_an_answering_run() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let (campaign, first) = campaign_of(stores, cells(&["a"], json!({}))).await?;
        let named = only_job(&first)?;
        finish_job_with_success(&*stores.submissions, &*stores.todo, &named, "eval").await?;
        let elsewhere = ingest_owned(
            &env,
            owner(USER_ADA)?,
            cells(&["a"], json!({"merge_duplicates": false})),
        )
        .await?;
        let other = only_job(&elsewhere)?;
        env.finish(&other).await?;

        let revision = revise_naming(
            stores,
            &campaign,
            &BTreeSet::new(),
            &BTreeSet::from([named.clone()]),
            cells(&["a"], json!({})),
        )
        .await?;

        let fresh = only_job(&revision)?;
        assert!(fresh != named && fresh != other, "{fresh}");
        assert!(revision.merges.is_empty(), "{:?}", revision.merges);
        Ok(())
    }

    /// A campaign plan keys a satisfied job under the digest of its unit, so
    /// the next revision carries the job.
    #[tokio::test]
    async fn a_revision_carries_a_satisfied_job() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let earlier = ingest_owned(&env, owner(USER_ADA)?, cells(&["a"], json!({}))).await?;
        let done = only_job(&earlier)?;
        env.finish(&done).await?;

        let (campaign, first) = campaign_of(stores, cells(&["a"], json!({}))).await?;
        assert_eq!(only_job(&first)?, done);
        let manifest = stores
            .plans
            .get_plan(&first.plan_id)
            .await?
            .context("manifest")?;
        assert!(
            manifest.content_keys.contains_key(&done),
            "{:?}",
            manifest.content_keys
        );

        let revision = revise(stores, &campaign, cells(&["a"], json!({}))).await?;

        assert_eq!(only_job(&revision)?, done);
        let kinds = merge_kinds(&revision);
        assert_eq!(kinds, vec![MergeKind::Carried]);
        Ok(())
    }

    /// Two inputs on one satisfied cell share the earlier job, and each counts
    /// as a merge.
    #[tokio::test]
    async fn twin_inputs_share_a_satisfied_job() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let earlier = ingest_owned(&env, owner(USER_ADA)?, cells(&["a"], json!({}))).await?;
        let done = only_job(&earlier)?;
        env.finish(&done).await?;

        let twins = ingest_owned(&env, owner(USER_ADA)?, cells(&["a", "a"], json!({}))).await?;

        assert!(
            twins.job_ids.iter().all(|(_, job_id)| *job_id == done),
            "{:?}",
            twins.job_ids
        );
        assert_eq!((twins.job_count, twins.merged_job_count), (1, 2));
        let kinds = merge_kinds(&twins);
        assert_eq!(kinds, vec![MergeKind::Satisfied, MergeKind::Satisfied]);
        Ok(())
    }

    /// The newest answering run wins.
    #[tokio::test]
    async fn the_newest_answering_run_satisfies_the_cell() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let catalog = &env.catalog;
        let opted_out = || cells(&["a"], json!({"merge_duplicates": false}));
        let older = only_job(&ingest(stores, catalog, None, opted_out()).await?)?;
        let newer = only_job(&ingest(stores, catalog, None, opted_out()).await?)?;
        assert_ne!(older, newer);
        futures::future::try_join_all([&older, &newer].map(|job_id| env.finish(job_id))).await?;

        let later = ingest(stores, catalog, None, cells(&["a"], json!({}))).await?;

        assert_eq!(only_job(&later)?, newer);
        Ok(())
    }

    /// A re-run mints even though its cell has a result, and the job it
    /// replaced satisfies no later cell.
    #[tokio::test]
    async fn a_rerun_mints_and_its_replaced_job_satisfies_nothing() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let (campaign, first) = campaign_of(stores, cells(&["a"], json!({}))).await?;
        let done = only_job(&first)?;
        env.finish(&done).await?;

        let rerun = rerun(
            stores,
            &campaign,
            &BTreeSet::from([done.clone()]),
            cells(&["a"], json!({})),
        )
        .await?;
        let again = only_job(&rerun)?;
        assert_ne!(again, done);
        assert!(rerun.merges.is_empty(), "{:?}", rerun.merges);

        let later = ingest(stores, &catalog()?, None, cells(&["a"], json!({}))).await?;
        assert_ne!(only_job(&later)?, done);
        assert!(
            later.merges.iter().all(|m| m.kind != MergeKind::Satisfied),
            "{:?}",
            later.merges
        );
        Ok(())
    }

    /// One run satisfies one unit of a plan: of two units on its cell, the
    /// other mints.
    #[tokio::test]
    async fn an_earlier_run_satisfies_one_unit_of_a_plan() -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let catalog = &env.catalog;
        let first = ingest(stores, catalog, None, cells(&["a"], json!({}))).await?;
        let done = only_job(&first)?;
        env.finish(&done).await?;

        let later = ingest(
            stores,
            catalog,
            None,
            vec![
                ("plain.json".into(), cell_body("a", json!({}))),
                (
                    "chip.json".into(),
                    cell_body("a", json!({"requires": ["os:macos", "chip:applem5"]})),
                ),
            ],
        )
        .await?;

        assert_eq!(later.job_count, 2);
        assert_eq!(later.job_ids[0].1, done);
        assert_ne!(later.job_ids[1].1, done);
        let kinds = merge_kinds(&later);
        assert_eq!(kinds, vec![MergeKind::Satisfied]);
        Ok(())
    }

    /// A revision cell whose carried job is still queued takes an earlier
    /// finished run in its place, unless it opted out of merging. The dry run
    /// of the revision draws what the filing takes.
    #[rstest]
    #[case::merges(json!({}), true)]
    #[case::opted_out(json!({"merge_duplicates": false}), false)]
    #[tokio::test]
    async fn a_queued_carry_gives_way_to_an_earlier_run(
        #[case] revised: Value,
        #[case] satisfied: bool,
    ) -> anyhow::Result<()> {
        let Some(env) = sql_env().await? else {
            return Ok(());
        };
        let stores = &env.stores;
        let (campaign, first) = campaign_of(stores, cells(&["a"], json!({}))).await?;
        let queued = only_job(&first)?;
        // Another campaign of the same owner, so the run is in scope.
        let elsewhere = ingest_owned(
            &env,
            owner(USER_ADA)?,
            cells(&["a"], json!({"merge_duplicates": false})),
        )
        .await?;
        let done = only_job(&elsewhere)?;
        env.finish(&done).await?;

        let jobs = cells(&["a"], revised);
        let draw = preview_draw(stores, &campaign, &jobs).await?;
        let revision = revise(stores, &campaign, jobs).await?;

        let expected = if satisfied {
            CarryDraw {
                satisfied: 1,
                ..CarryDraw::default()
            }
        } else {
            CarryDraw {
                carried: vec![queued.clone()],
                held: vec![HeldJob {
                    body_index: 0,
                    plan_id: first.plan_id.clone(),
                    job_id: queued.clone(),
                }],
                ..CarryDraw::default()
            }
        };
        assert_eq!(draw, expected);
        let (job_id, kind) = if satisfied {
            (done, MergeKind::Satisfied)
        } else {
            (queued, MergeKind::Carried)
        };
        assert_eq!(only_job(&revision)?, job_id);
        let kinds = merge_kinds(&revision);
        assert_eq!(kinds, vec![kind]);
        Ok(())
    }
}
