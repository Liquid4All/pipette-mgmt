//! `GET /jobs/{job_id}` (a device's submitted job) and
//! `GET /api/v1/plans/{plan_id}/jobs` (a plan's jobs and results), the one
//! surface that discloses a plan's `job_ids`; the plan read withholds them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::results::ResultRow;
use crate::{
    BenchmarkId, BenchmarkType, ChatMessage, ClientId, ClientRunSpec, JobId, NonEmptyTrimmedString,
    PlanId,
};

/// Where a submitted job stands.
#[derive(Debug, Clone, PartialEq, Eq, strum::EnumString, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum JobStatus {
    /// Recorded, waiting for the scorer.
    Incoming,
    /// With the scorer.
    Scoring,
    /// Scored; its metrics are readable.
    Processed,
    /// The device reported a failure.
    Failed,
    #[strum(default)]
    Unknown(String),
}

impl JobStatus {
    pub fn as_str(&self) -> &str {
        if let Self::Unknown(raw) = self {
            raw
        } else {
            self.into()
        }
    }
}

impl Serialize for JobStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JobStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(raw.parse().unwrap_or(Self::Unknown(raw)))
    }
}

/// `GET /jobs/{job_id}`'s body: a reported failure, or a result on its way
/// through scoring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JobRead {
    Failed(FailedJob),
    Submitted(SubmittedJob),
}

/// A job the device reported as failed; `status` is [`JobStatus::Failed`]. The
/// optional fields are `null` when the report left them out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedJob {
    pub job_id: JobId,
    pub benchmark_id: BenchmarkId,
    pub benchmark_type: BenchmarkType,
    pub status: JobStatus,
    pub submitted_at: DateTime<Utc>,
    pub failure_reason: NonEmptyTrimmedString,
    pub model_name: Option<NonEmptyTrimmedString>,
    pub model_quant: Option<NonEmptyTrimmedString>,
    pub runtime_name: Option<NonEmptyTrimmedString>,
    pub runtime_version: Option<NonEmptyTrimmedString>,
    pub client_version: Option<NonEmptyTrimmedString>,
}

/// A submitted result. `scored_at`, `score_runtime_version` and `metrics` are
/// `null` until it is scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmittedJob {
    pub job_id: JobId,
    pub benchmark_id: BenchmarkId,
    pub benchmark_type: BenchmarkType,
    pub status: JobStatus,
    pub submitted_at: DateTime<Utc>,
    pub scored_at: Option<DateTime<Utc>>,
    pub score_runtime_version: Option<String>,
    pub metrics: Option<Vec<JobMetric>>,
}

/// One scored metric of a job's latest scoring run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobMetric {
    pub metric: String,
    pub value: f32,
    pub value_stddev: Option<f32>,
    pub unit: String,
}

/// One sample of a scored eval, as `GET /jobs/{job_id}/eval-sample-results`
/// lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSampleResult {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub completion: String,
    pub is_correct: bool,
    pub failed: bool,
    pub failed_reason: Option<String>,
    /// The scorer's per-sample measurement where it reports one: an asr_eval
    /// sample's error counts (`hits`, `substitutions`, `deletions`,
    /// `insertions`, `reference_units`) and normalized `reference` /
    /// `hypothesis`. Absent for the accuracy evals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measures: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case("incoming", JobStatus::Incoming)]
    #[case("scoring", JobStatus::Scoring)]
    #[case("processed", JobStatus::Processed)]
    #[case("failed", JobStatus::Failed)]
    #[case("archived", JobStatus::Unknown("archived".into()))]
    fn a_job_status_keeps_its_spelling(#[case] raw: &str, #[case] status: JobStatus) {
        assert_eq!(
            serde_json::from_value::<JobStatus>(json!(raw)).ok(),
            Some(status.clone())
        );
        assert_eq!(serde_json::to_value(&status).ok(), Some(json!(raw)));
    }

    /// The two job bodies are told apart by `failure_reason`, which only a
    /// failure carries.
    #[test]
    fn a_job_read_tells_a_failure_from_a_result() -> Result<(), serde_json::Error> {
        let failed: JobRead = serde_json::from_value(json!({
            "job_id": "job-1", "benchmark_id": "b1", "benchmark_type": "eval",
            "status": "failed", "submitted_at": "2026-10-08T00:00:00Z",
            "failure_reason": "out of memory", "model_name": null, "model_quant": null,
            "runtime_name": null, "runtime_version": null, "client_version": null
        }))?;
        assert!(matches!(failed, JobRead::Failed(_)), "{failed:?}");

        let scored: JobRead = serde_json::from_value(json!({
            "job_id": "job-2", "benchmark_id": "b1", "benchmark_type": "eval",
            "status": "processed", "submitted_at": "2026-10-08T00:00:00Z",
            "scored_at": "2026-10-08T00:01:00Z", "score_runtime_version": "1",
            "metrics": [{"metric": "accuracy", "value": 0.5, "value_stddev": null, "unit": "ratio"}]
        }))?;
        assert!(matches!(scored, JobRead::Submitted(_)), "{scored:?}");
        Ok(())
    }
}

/// Where one of a plan's jobs stands: the vocabulary a plan's progress counts
/// in, plus `unknown` for a job the page's reads cannot settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for a client; no longer claimable once
    /// [`JobQueue::expires_at`] passes.
    Available,
    /// Held under a lease, still counted here once [`Lease::expires_at`] passes
    /// without a heartbeat.
    Running,
    /// Answered without a failure.
    Finished,
    /// Answered with a failure, or this plan's horizon for the job lapsed.
    Failed,
    /// Out of both queues unanswered: teardown retired it.
    Cancelled,
    /// Out of both queues with no run to settle it yet.
    Unknown,
}

/// Where a job stands in the queue: in `avail/`, or held under a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JobQueue {
    /// The deadline the queue holds the job under; `null` for none.
    pub expires_at: Option<DateTime<Utc>>,
    /// `null` while the job waits for a client.
    pub lease: Option<Lease>,
}

/// The lease a job is held under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Lease {
    /// When the lease lapses without a heartbeat.
    pub expires_at: DateTime<Utc>,
    /// The machine holding it, as the server handed it out. `null` at the
    /// withheld fleet tier.
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub client_id: Option<ClientId>,
}

/// What a job was filed to run: its spec as a record carries it, beside the
/// benchmark type filing gave it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanCell {
    #[serde(flatten)]
    pub spec: ClientRunSpec,
    /// `null` only for an id with no type prefix on a plan filed before its
    /// manifest recorded `benchmark_types`.
    pub benchmark_type: Option<BenchmarkType>,
}

/// A plan's own lapse record for a job: its deadline passed with no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ExpiredJob {
    /// This plan's deadline for the job, not the queue's `expires_at`, which
    /// is the latest across the plans that share the job.
    pub expires_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
}

/// What a job's plan records of it, in its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JobInPlan {
    /// This plan's outcome for the job, whatever else is true of a job another
    /// plan shares.
    pub lapsed: Option<ExpiredJob>,
    /// The job this one runs again.
    pub rerun_of: Option<ParentJob>,
    /// The runs before `rerun_of`, newest first, up to 16. Followed only with
    /// `results=true`, empty elsewhere.
    pub rerun_history: Vec<ParentJob>,
}

/// A job another one derives from, and the plan to find it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ParentJob {
    /// The job derived from.
    pub job_id: JobId,
    /// The newest plan of the same campaign holding it. `null` where none
    /// does, and without `results=true`.
    pub plan_id: Option<PlanId>,
}

/// A run a recalculation scored again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ParentRun {
    /// The run's job.
    pub job: ParentJob,
    /// One row per metric.
    pub rows: Vec<ResultRow>,
}

/// One of a plan's jobs: what it was filed to run, where it stands in the
/// queue, and what its plan records of it. What a run reported is
/// [`PlanJob::result`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Job {
    /// The job, as the plan lists it.
    pub job_id: JobId,
    /// Settled from every source below, and the run.
    pub state: JobState,
    /// The same in every state. `null` for a job whose spec is not stored or
    /// not readable.
    pub planned: Option<PlanCell>,
    /// `null` once the job leaves both queues.
    pub queue: Option<JobQueue>,
    /// What this plan records of the job.
    pub plan: JobInPlan,
}

/// A job's latest scoring run, or why there is none to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobResult {
    /// Its latest run, scored.
    Scored {
        /// One row per metric of the run, as the plan's results list them.
        rows: Vec<ResultRow>,
        /// The job whose recorded completions this run scores again.
        rescored_from: Option<ParentJob>,
        /// The runs before this one, newest first: from `rescored_from`'s,
        /// ending at a run that did not read, or the plan's `rerun_of`'s, each
        /// that has rows.
        earlier_runs: Vec<ParentRun>,
        /// A finished eval a recalculation takes; judged only on a page that
        /// names a `rerun_campaign_id`, `false` elsewhere.
        recalculable: bool,
    },
    /// Not answered: queued, running, cancelled, or lapsed for this plan.
    NotRun,
    /// It may have a run the page did not read.
    Unknown,
}

/// How a page's runs were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ResultsRead {
    /// The results store answered.
    Ok,
    /// The server reads no results.
    Off,
    /// The results store outlasted the read's timeout.
    TimedOut,
    /// The results store failed the read.
    Failed,
}

/// A [`Job`], and with `results=true` its latest scoring run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanJob {
    #[serde(flatten)]
    pub job: Job,
    /// Its latest run's rows, or why there are none. Absent unasked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    /// The digest of the planned runtime, as the scorer stores it for a row
    /// that reports the same runtime. It is the key of the runtime's catalog
    /// names in `descriptor_names`. Absent unasked, and for a job with no
    /// planned cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_runtime_sha256: Option<String>,
    /// The digest of the planned model, as the scorer stores it for a row that
    /// reports the same model. It is the key of the model's catalog names in
    /// `descriptor_names`. A planned vault model has the digest of the vault
    /// descriptor. A row records its Hugging Face alias. The two digests differ.
    /// This field is absent unless the caller requests it. It is also absent
    /// for a job with no planned cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_model_sha256: Option<String>,
}
