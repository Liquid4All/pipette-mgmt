//! A plan's result rows, one per metric of each scored run, as the jobs read
//! with `results=true` and `pipette-mgmt plans results` carry them.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    BenchmarkFlags, ElapsedMillis, FleetDetail, JobId, Model, ModelFlags, PlanId, Runtime,
    RuntimeFlags,
};

/// Where a failed run's failure came from.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
    strum::EnumIter,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FailureSource {
    /// The client reported it.
    Client,
    /// The job's deadline passed unanswered.
    Expired,
    /// Every client that could take it refused it.
    AllClientsDenied,
    /// No client could ever match it.
    Unmatchable,
    /// Its lease was lost too many times.
    LeaseLost,
}

/// A stored value as the type it decodes to, or as stored where it does not
/// (not JSON, a shape or variant this build does not know).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
#[serde(untagged)]
pub enum OrUnknown<T> {
    Known(T),
    Unknown(UnknownValue),
}

/// `{"type": "unknown", "value": ...}`: the stored JSON, or the stored text as
/// a string where it is not JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UnknownValue {
    pub r#type: UnknownTag,
    pub value: Value,
}

impl UnknownValue {
    pub fn new(value: Value) -> Self {
        Self {
            r#type: UnknownTag::Unknown,
            value,
        }
    }
}

/// The one tag an [`UnknownValue`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UnknownTag {
    Unknown,
}

/// One metric of one scored run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ResultRow {
    /// The job the run answered.
    pub job_id: JobId,
    /// The benchmark the job ran, by its catalog id, such as
    /// `prefill_throughput_256`.
    pub benchmark_id: String,
    /// The benchmark's kind, such as `prefill_throughput`.
    pub benchmark_type: String,
    /// What the row measures, such as `ttft`.
    pub metric: String,
    /// The measurement, in `unit`; `null` on a failure row.
    pub value: Option<f32>,
    /// The measurement's standard deviation over the run's repetitions;
    /// `null` on a failure row, and where the metric has none.
    pub value_stddev: Option<f32>,
    /// The unit of `value`, such as `ms` or `tokens/sec`.
    pub unit: String,
    /// `null` on a row written before the column existed, which was no failure.
    pub failed: Option<bool>,
    /// Why the run failed, as free text; `null` at the `withheld` fleet detail.
    pub failed_reason: Option<String>,
    /// The model the client reported.
    pub model_descriptor: Option<OrUnknown<Model>>,
    /// The digest of `model_descriptor`, as the warehouse stores it. For a
    /// vault run, `model_descriptor` is the model's Hugging Face alias, and the
    /// digest is the alias's. It is the key of the model's catalog names in
    /// `descriptor_names`. `null` on a row with no model descriptor.
    pub model_descriptor_sha256: Option<String>,
    /// The runtime the client reported.
    pub runtime_descriptor: Option<OrUnknown<Runtime>>,
    /// The digest of `runtime_descriptor`, as the warehouse stores it. The
    /// digest leaves out a vault `custom_label`. It is the key of the runtime's
    /// catalog names in `descriptor_names`. `null` on a row written before the
    /// warehouse stored the digest.
    pub runtime_descriptor_sha256: Option<String>,
    /// The vault model the client reported, when `model_descriptor` is its
    /// HuggingFace alias; `null` on a run that loaded no vault model.
    pub model_vault_descriptor: Option<OrUnknown<Model>>,
    /// The model flags the run executed with, as its client reported them,
    /// with the row's axes added.
    pub model_flags: Option<OrUnknown<ModelFlags>>,
    /// The runtime flags the run executed with, as its client reported them.
    pub runtime_flags: Option<OrUnknown<RuntimeFlags>>,
    /// The benchmark flags the run executed with, as its client reported them.
    pub benchmark_flags: Option<OrUnknown<BenchmarkFlags>>,
    /// The model flags the job asked for, read from the job handed out; `null`
    /// where it named none.
    pub requested_model_flags: Option<OrUnknown<ModelFlags>>,
    /// The runtime flags the job asked for.
    pub requested_runtime_flags: Option<OrUnknown<RuntimeFlags>>,
    /// The benchmark flags the job asked for.
    pub requested_benchmark_flags: Option<OrUnknown<BenchmarkFlags>>,
    /// The run's metadata, such as `samples_failed` on an eval. This and
    /// `client_extras` are a map where the stored value is one, else the
    /// unknown wrapper, which a map reader cannot tell from a map that spells
    /// one.
    pub eval_metadata: Option<OrUnknown<Map<String, Value>>>,
    /// What the client reported that no other field names; `null` at the
    /// `withheld` fleet detail.
    pub client_extras: Option<OrUnknown<Map<String, Value>>>,
    /// The run's parameters by name: the row's non-null `parameter_*` columns.
    pub parameters: BTreeMap<String, Value>,
    /// What the run measured beside its metrics, such as its peak swap: the
    /// row's non-null `observation_*` columns.
    pub observations: BTreeMap<String, Value>,
    /// The device that ran the job; `null` at the `withheld` fleet detail, and
    /// on a failure the server declared, which no device ran.
    pub client_id: Option<String>,
    /// The device's name; `null` where `client_id` is.
    pub device_name: Option<String>,
    /// The device's state and make as the client read them; empty at the
    /// `withheld` fleet detail.
    pub device: BTreeMap<String, Value>,
    /// The client build that ran the job; `null` at the `withheld` fleet
    /// detail.
    pub client_version: Option<String>,
    /// When the client submitted the run.
    pub submitted_at: Option<DateTime<Utc>>,
    /// When the server scored the run.
    pub scored_at: Option<DateTime<Utc>>,
    /// The milliseconds the device spent on the job, as its client reported
    /// them; every row of one run carries the same value.
    pub device_time_ms: Option<ElapsedMillis>,
    /// Which pass declared the failure; `null` on a measured row, and on one
    /// whose source this build does not know.
    #[serde(default, deserialize_with = "known_source")]
    pub failure_source: Option<FailureSource>,
    /// The job whose recorded completions a re-scored copy scores again;
    /// `null` on any other row.
    pub rescored_from: Option<JobId>,
    /// When a device submitted the run that the row scores. It equals
    /// `submitted_at`, except on a re-scored copy. There it is the time of the
    /// run that the copy scores again. `null` on a row scored before the
    /// warehouse recorded it, and on a re-scored copy whose chain of copies
    /// reaches a record that is gone or is not a success.
    pub run_submitted_at: Option<DateTime<Utc>>,
    /// The version of the scoring service that scored the row: the
    /// `runtime_version` of its `POST /score` answer. Only an eval row has a
    /// value. It is `null` on every other row and on an eval row whose scoring
    /// failed.
    pub score_runtime_version: Option<String>,
    /// The model's parameter counts in millions, as the catalog had them when
    /// the row was scored or `fix-model-param` filled them in, else from the
    /// catalog when the results are read; `null` while it has no entry.
    pub model_params_total_millions: Option<i32>,
    /// Of those, the parameters a token activates: the total for a dense
    /// model, fewer for a mixture of experts.
    pub model_params_active_millions: Option<i32>,
}

/// A page of a plan's results, as `pipette-mgmt plans results --json` prints it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ResultListView {
    pub fleet_detail: FleetDetail,
    pub plan_id: PlanId,
    /// Jobs the manifest lists.
    pub jobs: usize,
    /// Of those, the jobs with at least one result row.
    pub jobs_reported: usize,
    /// Result rows before paging.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    /// One row per metric of each job's latest scoring run, in the plan's job
    /// order. At the withheld fleet tier any `failure_source` but `expired` and
    /// `lease_lost` is `null`.
    pub results: Vec<ResultRow>,
}

/// A source this build does not know reads as none rather than failing the row.
fn known_source<'de, D>(deserializer: D) -> Result<Option<FailureSource>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| FailureSource::deserialize(value).ok()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A known value is the type's own wire form; an unknown one is tagged, and
    /// reads back as unknown rather than as a variant.
    #[test]
    fn or_unknown_keeps_each_side_s_wire_form() -> anyhow::Result<()> {
        let known: OrUnknown<Model> =
            serde_json::from_value(json!({"type": "apple_foundation_text"}))?;
        assert_eq!(known, OrUnknown::Known(Model::AppleFoundationText));
        assert_eq!(
            serde_json::to_value(&known)?,
            json!({"type": "apple_foundation_text"})
        );
        let unknown = OrUnknown::<Model>::Unknown(UnknownValue::new(json!("enable_thinking=true")));
        let wire = serde_json::to_value(&unknown)?;
        assert_eq!(
            wire,
            json!({"type": "unknown", "value": "enable_thinking=true"})
        );
        assert_eq!(serde_json::from_value::<OrUnknown<Model>>(wire)?, unknown);
        Ok(())
    }
}
