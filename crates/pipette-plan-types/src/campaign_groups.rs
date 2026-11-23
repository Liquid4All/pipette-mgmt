//! `/api/v1/campaign-groups`: campaigns of one owner listed together, and the
//! data-quality policies kept on them.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    CampaignGroup, CampaignGroupId, CampaignId, CampaignView, FailureCode, FailureSource, JobId,
    NonEmptyTrimmedString, PlanId, PolicyKind, UserId, VaultPrincipalId,
};

/// A campaign group and its members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignGroupView {
    #[serde(flatten)]
    pub group: CampaignGroup,
    /// The members the reader may read, in campaign id order, which is
    /// creation order.
    pub members: Vec<CampaignView>,
}

/// `POST /api/v1/campaign-groups` request body. Unknown keys are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateCampaignGroupRequest {
    /// At most 4096 characters.
    pub name: NonEmptyTrimmedString,
    /// Who owns the group, a `user_…` or `org_…` id; absent, whoever signs
    /// the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<VaultPrincipalId>,
}

/// `PATCH /api/v1/campaign-groups/{group_id}` request body. Unknown keys are
/// refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateCampaignGroupRequest {
    /// At most 4096 characters.
    pub name: NonEmptyTrimmedString,
}

/// `POST /api/v1/campaign-groups/{group_id}/members` request body. Unknown
/// keys are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AddCampaignGroupMembersRequest {
    /// At least one; a campaign named twice joins once.
    pub campaign_ids: Vec<CampaignId>,
}

/// A policy's settings, by kind: `spread` takes `percent`, and optionally
/// `short_repetition_ms` with `short_repetition_percent`; `failed` takes
/// `codes`; the others none. Unknown keys are refused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PolicySettings {
    /// `spread`'s threshold, a finite number above 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    /// `spread` weighs a result row whose repetitions take under this many
    /// milliseconds on average against `short_repetition_percent` instead of
    /// `percent`. A finite number above 0, set together with
    /// `short_repetition_percent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_repetition_ms: Option<f64>,
    /// `spread`'s threshold for a row under `short_repetition_ms`, a finite
    /// number above 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_repetition_percent: Option<f64>,
    /// `failed` re-runs a run whose device reported a failure with one of
    /// these codes, at most the code's number of times, from 1 to
    /// `MAX_RERUNS`. At least one code; a code left out is never re-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codes: Option<BTreeMap<FailureCode, u32>>,
}

/// Why settings do not suit their policy's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicySettingsError {
    #[error("a spread policy takes settings.percent, a finite number above 0")]
    Percent,
    #[error(
        "a spread policy takes settings.short_repetition_ms and \
         settings.short_repetition_percent together, each a finite number above 0"
    )]
    ShortRepetition,
    #[error(
        "a failed policy takes settings.codes alone, at least one failure code, each with its \
         re-runs, 1 to {}",
        MAX_RERUNS
    )]
    Codes,
    #[error("a {0} policy takes no settings.codes, only a failed policy does")]
    UnexpectedCodes(PolicyKind),
    #[error("a {0} policy takes no settings")]
    Unexpected(PolicyKind),
}

/// A campaign group's data-quality policy, one per group and kind. While it is
/// on, every queue-maintenance run applies it to each member campaign's
/// current plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignGroupPolicy {
    pub group_id: CampaignGroupId,
    pub kind: PolicyKind,
    pub enabled: bool,
    /// The most re-runs a flagged cell gets, from `PolicyKind::min_reruns` to
    /// `PolicyKind::max_reruns`; 0 for failed, whose `settings.codes` holds
    /// them.
    #[cfg_attr(feature = "schemars", schemars(range(max = MAX_RERUNS)))]
    pub max_reruns: u32,
    pub settings: PolicySettings,
    /// Who turned the policy on; `null` while it is off.
    pub enabled_by: Option<UserId>,
    /// When the policy was turned on; `null` while it is off.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub enabled_at: Option<Timestamp>,
}

/// The most re-runs a policy gives a cell, so a cell's re-run chain fits the 16
/// links the job history read follows (`HISTORY_LIMIT`).
pub const MAX_RERUNS: u32 = 15;

impl PolicyKind {
    /// This kind's policy before any write.
    pub fn defaults(self, group_id: CampaignGroupId) -> CampaignGroupPolicy {
        let (max_reruns, settings) = match self {
            Self::Swap => (5, PolicySettings::default()),
            Self::Spread => (
                3,
                PolicySettings {
                    percent: Some(5.0),
                    short_repetition_ms: Some(1000.0),
                    short_repetition_percent: Some(10.0),
                    codes: None,
                },
            ),
            Self::Failed => (
                0,
                PolicySettings {
                    // Another device may run a harness_error (a phone's failed
                    // install, a client build that does not know the benchmark);
                    // an out of memory may come from memory another process
                    // holds; attempt_timeout is left out because every device
                    // has the same limit.
                    codes: Some(
                        [
                            (FailureCode::RuntimeError, 2),
                            (FailureCode::DeviceError, 2),
                            (FailureCode::Unspecified, 2),
                            (FailureCode::DeviceNotReady, 1),
                            (FailureCode::HarnessError, 1),
                            (FailureCode::OutOfMemory, 1),
                        ]
                        .into(),
                    ),
                    ..PolicySettings::default()
                },
            ),
            Self::TtftOverE2e => (3, PolicySettings::default()),
            Self::ModelParams | Self::SmallerFailed | Self::VaultResource => {
                (0, PolicySettings::default())
            }
        };
        CampaignGroupPolicy {
            group_id,
            kind: self,
            enabled: false,
            max_reruns,
            settings,
            enabled_by: None,
            enabled_at: None,
        }
    }

    /// The fewest re-runs this kind's policy takes. A swap policy marks a cell
    /// still flagged failed, so it acts with none; spread marks one too, after
    /// at least 1 re-run; ttft_over_e2e keeps the cell's result, so with none
    /// it would do nothing. Failed takes 0, as its `settings.codes` holds its
    /// re-runs.
    pub fn min_reruns(self) -> u32 {
        match self {
            Self::Swap
            | Self::Failed
            | Self::ModelParams
            | Self::SmallerFailed
            | Self::VaultResource => 0,
            Self::Spread | Self::TtftOverE2e => 1,
        }
    }

    /// The most re-runs this kind's policy takes.
    pub fn max_reruns(self) -> u32 {
        match self {
            Self::Swap | Self::Spread | Self::TtftOverE2e => MAX_RERUNS,
            Self::Failed | Self::ModelParams | Self::SmallerFailed | Self::VaultResource => 0,
        }
    }

    /// Whether this kind's policy re-runs or marks jobs, which queue-maintenance
    /// files with the host's authority; `model_params`, `smaller_failed` and
    /// `vault_resource` only flag.
    pub fn reruns_or_marks(self) -> bool {
        match self {
            Self::Swap | Self::Spread | Self::Failed | Self::TtftOverE2e => true,
            Self::ModelParams | Self::SmallerFailed | Self::VaultResource => false,
        }
    }

    /// Whether this kind's policy marks a cell failed once its re-runs are
    /// spent and its check still flags the cell; failed and ttft_over_e2e keep
    /// the cell's result.
    pub fn marks(self) -> bool {
        match self {
            Self::Swap | Self::Spread => true,
            Self::Failed
            | Self::TtftOverE2e
            | Self::ModelParams
            | Self::SmallerFailed
            | Self::VaultResource => false,
        }
    }

    /// This kind's policy among a group's `stored` ones, else its defaults.
    pub fn policy(
        self,
        group_id: &CampaignGroupId,
        stored: &[CampaignGroupPolicy],
    ) -> CampaignGroupPolicy {
        stored
            .iter()
            .find(|policy| policy.kind == self)
            .cloned()
            .unwrap_or_else(|| self.defaults(group_id.clone()))
    }

    /// Refuses settings this kind does not take.
    pub fn check_settings(self, settings: &PolicySettings) -> Result<(), PolicySettingsError> {
        // Destructured, so a new setting has to be weighed for every kind.
        let PolicySettings {
            percent,
            short_repetition_ms,
            short_repetition_percent,
            ref codes,
        } = *settings;
        let positive = |n: f64| n.is_finite() && n > 0.0;
        let numbers = [percent, short_repetition_ms, short_repetition_percent];
        match self {
            Self::Failed => match codes {
                Some(codes)
                    if numbers.iter().all(Option::is_none)
                        && !codes.is_empty()
                        && codes.values().all(|n| (1..=MAX_RERUNS).contains(n)) =>
                {
                    Ok(())
                }
                _ => Err(PolicySettingsError::Codes),
            },
            _ if codes.is_some() => Err(PolicySettingsError::UnexpectedCodes(self)),
            Self::Spread if !percent.is_some_and(positive) => Err(PolicySettingsError::Percent),
            Self::Spread => match (short_repetition_ms, short_repetition_percent) {
                (None, None) => Ok(()),
                (Some(ms), Some(short)) if positive(ms) && positive(short) => Ok(()),
                _ => Err(PolicySettingsError::ShortRepetition),
            },
            Self::Swap
            | Self::TtftOverE2e
            | Self::ModelParams
            | Self::SmallerFailed
            | Self::VaultResource => {
                if numbers.iter().all(Option::is_none) {
                    Ok(())
                } else {
                    Err(PolicySettingsError::Unexpected(self))
                }
            }
        }
    }
}

/// `GET /api/v1/campaign-groups/{group_id}/policies` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignGroupPolicies {
    /// One per kind, in kind order.
    pub policies: Vec<CampaignGroupPolicy>,
}

/// `PUT /api/v1/campaign-groups/{group_id}/policies/{kind}` request body: the
/// whole policy, and the policy it was read from. Unknown keys are refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetCampaignGroupPolicyRequest {
    pub enabled: bool,
    #[cfg_attr(feature = "schemars", schemars(range(max = MAX_RERUNS)))]
    pub max_reruns: u32,
    pub settings: PolicySettings,
    /// The policy as the caller read it, its kind's defaults if none was
    /// written. A write other than a Turn off is refused unless it is still
    /// the stored policy.
    pub base: PolicyBase,
}

/// A policy's values as a write's caller read them. Unknown keys are refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PolicyBase {
    pub enabled: bool,
    pub max_reruns: u32,
    pub settings: PolicySettings,
}

/// `GET /api/v1/campaign-groups/{group_id}/policy-preview` body: what the
/// policies would do now. The server files, writes and marks nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignGroupPolicyPreview {
    /// One per member whose plans the reader may read, in member order.
    pub members: Vec<MemberPolicyPreview>,
}

/// What a group's policies would re-run and mark in one member campaign's
/// current plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct MemberPolicyPreview {
    pub campaign_id: CampaignId,
    /// The plan the campaign runs; `null` for none.
    pub plan_id: Option<PlanId>,
    pub state: PolicyMemberState,
    /// The plan's jobs whose outcome queue-maintenance recorded, an eval's left
    /// out, as no policy judges one; 0 unless `state` is `evaluated` or
    /// `unreadable`.
    pub answered_job_count: usize,
    /// Answered jobs with no rows in pipette-duckdb yet, which wait.
    pub waiting_job_count: usize,
    /// The earliest deadline of the plan's jobs; `null` for none, and when
    /// `state` is neither `evaluated` nor `unreadable`.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub earliest_deadline: Option<Timestamp>,
    /// Whether `earliest_deadline` had passed at the read. If the plan's
    /// document sets it, the server refuses a re-run of the plan.
    pub deadline_passed: bool,
    /// One per kind, in kind order.
    pub checks: Vec<PolicyCheckCounts>,
    /// The jobs the policies that are on would re-run.
    pub rerun_job_ids: BTreeSet<JobId>,
    /// The jobs they would mark failed.
    pub marked_job_ids: BTreeSet<JobId>,
    /// Answered jobs with rows they leave as they are, those none of them
    /// flags included.
    pub kept_job_count: usize,
    /// Flagged jobs they leave aside, each with why.
    pub skipped: BTreeMap<JobId, PolicySkipReason>,
    /// The plan of the last group filing that holds the member while `state`
    /// is `waiting_for_devices` or `filing_not_advanced`; `null` otherwise.
    pub filing_plan_id: Option<PlanId>,
    /// The jobs that filing queued that no device has claimed; 0 unless
    /// `state` is `waiting_for_devices`.
    pub unclaimed_job_count: usize,
    /// The answered jobs' result rows that the pages show without a total or
    /// active parameter count, by the key a model params entry would take, in
    /// key order; empty unless `state` is `evaluated`.
    pub missing_model_params: Vec<ModelParamsGap>,
    /// The answered jobs that failed while a larger job of their family has
    /// rows and no failure, in benchmark id order; empty unless `state` is
    /// `evaluated`, `waiting_for_devices` or `refused`.
    pub smaller_failed: Vec<SmallerFailure>,
    /// The policies' last revision of the campaign; `null` for none.
    pub last_filing: Option<PolicyFiling>,
}

/// A failed job of a member, and the smallest of the larger jobs of its family,
/// the cells that differ from it only in their sizes, that has rows and no
/// failure. Every size of `clean_benchmark_id` is at least that of
/// `benchmark_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SmallerFailure {
    pub job_id: JobId,
    pub benchmark_id: String,
    /// The failure the job's first failed row records.
    pub failure_source: Option<FailureSource>,
    pub failure_code: Option<FailureCode>,
    /// The model of the clean job's rows, by the key a model params entry
    /// takes; `null` where they name none.
    pub model_key: Option<String>,
    pub clean_job_id: JobId,
    pub clean_benchmark_id: String,
}

/// A member's result rows that lack model params, under the key an entry for
/// their model would take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ModelParamsGap {
    pub key: String,
    pub row_count: usize,
    pub job_count: usize,
}

/// A revision queue-maintenance filed into a member campaign for its group's
/// policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PolicyFiling {
    pub plan_id: PlanId,
    #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
    pub filed_at: Timestamp,
    /// The jobs the plan holds that the plan it revised did not: its re-runs,
    /// its marks, and the lapsed cells it placed afresh.
    pub queued_job_count: usize,
}

/// How far a member campaign was read for its group's policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolicyMemberState {
    /// Its plan's answered jobs were checked.
    Evaluated,
    /// Its owner is not the group's, so no policy acts on it.
    OtherOwner,
    /// It is archived, runs no plan, or runs one filed without a document.
    NoPlan,
    /// Its plan is still `creating`.
    NotReady,
    /// Someone stopped its plan: `cancelling`, `cancelled`, or named by a
    /// cancel marker.
    Cancelled,
    /// pipette-duckdb failed or timed out on its rows, or on an earlier
    /// member's.
    Unreadable,
    /// Jobs its last group filing queued wait for a device, so nothing more
    /// is read until devices take them.
    WaitingForDevices,
    /// Its last group filing did not move the campaign onto its plan, which
    /// still runs.
    FilingNotAdvanced,
    /// Its plan cannot be filed again under this release and the same
    /// policies: its document is gone, does not match, or was refused, such as
    /// for a passed deadline.
    Refused,
}

/// What one policy alone, with its own settings, does with a member's
/// answered jobs, whether it is on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PolicyCheckCounts {
    pub kind: PolicyKind,
    /// Whether the policy is on.
    pub on: bool,
    /// Jobs the check flags, the skipped ones included.
    pub flagged: usize,
    pub rerun: usize,
    pub marked: usize,
    pub kept: usize,
}

/// Query parameters for `GET /api/v1/campaign-groups/{group_id}/rerun-results`,
/// paged as `GET /api/v1/plans/{plan_id}/jobs` pages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RerunResultsQuery {
    /// Rows a page holds: 100 by default, at most 500. `0` answers `summary`
    /// and `total` alone.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// Rows to skip, newest filing first.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub offset: Option<usize>,
    /// Every row in one page, whatever `limit` and `offset` say.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub all: Option<bool>,
}

/// `GET /api/v1/campaign-groups/{group_id}/rerun-results` body: the re-runs
/// the group's policies filed in the members whose plans the reader may read,
/// and how each answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CampaignGroupRerunResults {
    /// One per policy with a re-run, in kind order, over every row.
    pub summary: Vec<PolicyRerunSummary>,
    /// Rows before paging.
    pub total: usize,
    /// Rows skipped before the page; `0` with `all=true`.
    pub offset: usize,
    /// The most rows the page holds, once clamped; `total` with `all=true`.
    pub limit: usize,
    /// The page, newest filing first.
    pub rows: Vec<PolicyRerun>,
}

/// One policy's re-runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PolicyRerunSummary {
    pub kind: PolicyKind,
    pub reruns: usize,
    /// Re-runs that wait for their rows.
    pub waiting: usize,
    pub cleared: usize,
    pub flagged: usize,
    pub failed: usize,
    pub dropped: usize,
    /// The medians of `before` and `after` over the cleared, flagged and
    /// failed re-runs, the ones whose rows were read; `null` for none.
    pub median_before: Option<f64>,
    pub median_after: Option<f64>,
}

/// A re-run that one policy of a campaign group filed: its check's value on
/// the run it replaces, and on the re-run once its rows land.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PolicyRerun {
    pub campaign_id: CampaignId,
    pub kind: PolicyKind,
    /// The job the filing queued.
    pub rerun_job_id: JobId,
    /// The run it replaces.
    pub rerun_of: JobId,
    /// The plan the filing filed.
    pub plan_id: PlanId,
    #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
    pub filed_at: Timestamp,
    /// The runs in a row the policy flagged, the replaced one included: 1 for
    /// a job's first re-run, 2 for a re-run of a re-run.
    pub attempt: u32,
    pub benchmark_id: String,
    /// The key the run's model takes in the model params table; `null` when
    /// its rows name no model.
    pub model_key: Option<String>,
    /// The check's value on the replaced run: the largest swap in bytes, the
    /// largest spread or TTFT share of E2E as a percent, or for failed 1.
    pub before: f64,
    /// What the check weighs the value against, as the policy stood at the
    /// filing: 0 for swap, `percent` for spread, 100 for ttft_over_e2e; `null`
    /// for failed, which flags 1.
    pub threshold: Option<f64>,
    /// Spread's `short_repetition_ms` at the filing; `null` when it had no
    /// short repetition rule, and for the other kinds.
    pub short_repetition_ms: Option<f64>,
    /// Spread's `short_repetition_percent` at the filing, `null` with
    /// `short_repetition_ms`.
    pub short_repetition_percent: Option<f64>,
    /// The codes of failed's `codes` at the filing, which the re-run is judged
    /// by; `null` for the other kinds.
    pub codes: Option<Vec<FailureCode>>,
    /// For failed, the replaced run's failure code.
    pub before_code: Option<String>,
    /// The check's value on the re-run; `null` while it waits, once it is
    /// dropped, and when its rows hold none.
    pub after: Option<f64>,
    /// For failed, the re-run's failure code; `null` when it did not fail.
    pub after_code: Option<String>,
    /// `null` while the re-run waits for its rows.
    pub outcome: Option<RerunOutcome>,
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>", extend("format" = "date-time")))]
    pub judged_at: Option<Timestamp>,
}

/// What a re-run's check reads on its rows, at the threshold it was filed at,
/// or that the campaign dropped the re-run before it had a recorded result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::AsRefStr)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RerunOutcome {
    /// The check no longer flags the re-run, and none of its rows failed.
    Cleared,
    /// The check still flags it.
    Flagged,
    /// The check no longer flags it, but a row of it failed.
    Failed,
    /// The campaign no longer runs it, and it has neither rows nor a recorded
    /// result: a later revision dropped it, or a person marked it.
    Dropped,
}

/// Why a flagged job is neither re-run, marked nor kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolicySkipReason {
    /// The plan holds no content key for it (filed before digests were
    /// recorded), so a revision can neither re-run nor mark it.
    NoContentKey,
    /// The read of the plans holding an earlier run failed; it is read again
    /// next time.
    ChainReadFailed,
    /// An earlier run has no rows yet; it is read again next time.
    EarlierRunWithoutRows,
    /// Its re-run chain runs through a plan that does not decode, so it is
    /// left as it is.
    UnreadablePlan,
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;

    /// Each kind's default re-runs and settings, which pass its check, its
    /// range, and whether it re-runs or marks, in kind order: model_params,
    /// smaller_failed and vault_resource come last, off with no re-run and no
    /// settings, and take neither.
    #[test]
    fn model_params_takes_no_rerun_and_no_setting() -> anyhow::Result<()> {
        let group_id = CampaignGroupId::try_new("group_x")?;
        let limits = PolicyKind::iter()
            .map(|kind| {
                let defaults = kind.defaults(group_id.clone());
                kind.check_settings(&defaults.settings)?;
                Ok((
                    kind,
                    defaults.max_reruns,
                    defaults.settings,
                    kind.min_reruns(),
                    kind.max_reruns(),
                    kind.reruns_or_marks(),
                ))
            })
            .collect::<Result<Vec<_>, PolicySettingsError>>()?;
        let none = settings(None, None, None);
        let short = settings(Some(5.0), Some(1000.0), Some(10.0));
        let failed = codes(&[
            (FailureCode::RuntimeError, 2),
            (FailureCode::DeviceError, 2),
            (FailureCode::Unspecified, 2),
            (FailureCode::DeviceNotReady, 1),
            (FailureCode::HarnessError, 1),
            (FailureCode::OutOfMemory, 1),
        ]);
        assert_eq!(
            limits,
            [
                (PolicyKind::Swap, 5, none.clone(), 0, MAX_RERUNS, true),
                (PolicyKind::Spread, 3, short, 1, MAX_RERUNS, true),
                (PolicyKind::Failed, 0, failed, 0, 0, true),
                (
                    PolicyKind::TtftOverE2e,
                    3,
                    none.clone(),
                    1,
                    MAX_RERUNS,
                    true
                ),
                (PolicyKind::ModelParams, 0, none.clone(), 0, 0, false),
                (PolicyKind::SmallerFailed, 0, none.clone(), 0, 0, false),
                (PolicyKind::VaultResource, 0, none.clone(), 0, 0, false),
            ]
        );
        let kind = PolicyKind::ModelParams;
        assert_eq!(serde_json::to_value(kind)?, "model_params");
        let defaults = kind.defaults(group_id);
        assert_eq!((defaults.enabled, &defaults.settings), (false, &none));
        assert_eq!(kind.check_settings(&none), Ok(()));
        assert_eq!(
            kind.check_settings(&settings(Some(5.0), None, None)),
            Err(PolicySettingsError::Unexpected(kind))
        );
        Ok(())
    }

    fn settings(
        percent: Option<f64>,
        short_repetition_ms: Option<f64>,
        short_repetition_percent: Option<f64>,
    ) -> PolicySettings {
        PolicySettings {
            percent,
            short_repetition_ms,
            short_repetition_percent,
            codes: None,
        }
    }

    fn codes(codes: &[(FailureCode, u32)]) -> PolicySettings {
        PolicySettings {
            codes: Some(codes.iter().copied().collect()),
            ..PolicySettings::default()
        }
    }

    /// Spread takes a percent, and a short repetition's time and percent only
    /// together; failed takes codes alone, at least one, any code among them,
    /// each with 1 to `MAX_RERUNS` re-runs; the other kinds take none of these.
    #[rstest::rstest]
    #[case::percent(PolicyKind::Spread, settings(Some(5.0), None, None), Ok(()))]
    #[case::short(PolicyKind::Spread, settings(Some(5.0), Some(1000.0), Some(10.0)), Ok(()))]
    #[case::no_percent(
        PolicyKind::Spread,
        settings(None, Some(1000.0), Some(10.0)),
        Err(PolicySettingsError::Percent)
    )]
    #[case::time_alone(
        PolicyKind::Spread,
        settings(Some(5.0), Some(1000.0), None),
        Err(PolicySettingsError::ShortRepetition)
    )]
    #[case::percent_alone(
        PolicyKind::Spread,
        settings(Some(5.0), None, Some(10.0)),
        Err(PolicySettingsError::ShortRepetition)
    )]
    #[case::zero_time(
        PolicyKind::Spread,
        settings(Some(5.0), Some(0.0), Some(10.0)),
        Err(PolicySettingsError::ShortRepetition)
    )]
    #[case::infinite_percent(
        PolicyKind::Spread,
        settings(Some(5.0), Some(1000.0), Some(f64::INFINITY)),
        Err(PolicySettingsError::ShortRepetition)
    )]
    #[case::swap(
        PolicyKind::Swap,
        settings(None, Some(1000.0), Some(10.0)),
        Err(PolicySettingsError::Unexpected(PolicyKind::Swap))
    )]
    #[case::failed_one(PolicyKind::Failed, codes(&[(FailureCode::HarnessError, 1)]), Ok(()))]
    #[case::failed_every(
        PolicyKind::Failed,
        codes(&FailureCode::iter().map(|code| (code, MAX_RERUNS)).collect::<Vec<_>>()),
        Ok(())
    )]
    #[case::failed_none(
        PolicyKind::Failed,
        PolicySettings::default(),
        Err(PolicySettingsError::Codes)
    )]
    #[case::failed_empty(PolicyKind::Failed, codes(&[]), Err(PolicySettingsError::Codes))]
    #[case::failed_no_reruns(
        PolicyKind::Failed,
        codes(&[(FailureCode::DeviceError, 2), (FailureCode::HarnessError, 0)]),
        Err(PolicySettingsError::Codes)
    )]
    #[case::failed_too_many_reruns(
        PolicyKind::Failed,
        codes(&[(FailureCode::DeviceError, MAX_RERUNS + 1)]),
        Err(PolicySettingsError::Codes)
    )]
    #[case::failed_with_a_percent(
        PolicyKind::Failed,
        PolicySettings { percent: Some(5.0), ..codes(&[(FailureCode::DeviceError, 2)]) },
        Err(PolicySettingsError::Codes)
    )]
    #[case::codes_on_spread(
        PolicyKind::Spread,
        PolicySettings { percent: Some(5.0), ..codes(&[(FailureCode::DeviceError, 2)]) },
        Err(PolicySettingsError::UnexpectedCodes(PolicyKind::Spread))
    )]
    #[case::codes_on_swap(
        PolicyKind::Swap,
        codes(&[(FailureCode::DeviceError, 2)]),
        Err(PolicySettingsError::UnexpectedCodes(PolicyKind::Swap))
    )]
    #[case::codes_on_smaller_failed(
        PolicyKind::SmallerFailed,
        codes(&[(FailureCode::DeviceError, 2)]),
        Err(PolicySettingsError::UnexpectedCodes(PolicyKind::SmallerFailed))
    )]
    #[case::percent_on_vault_resource(
        PolicyKind::VaultResource,
        settings(Some(5.0), None, None),
        Err(PolicySettingsError::Unexpected(PolicyKind::VaultResource))
    )]
    fn each_kind_takes_its_own_settings(
        #[case] kind: PolicyKind,
        #[case] settings: PolicySettings,
        #[case] checked: Result<(), PolicySettingsError>,
    ) {
        assert_eq!(kind.check_settings(&settings), checked);
    }
}
