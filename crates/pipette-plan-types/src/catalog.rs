//! The catalog a device reads: `GET /benchmarks`, an entry and an eval's
//! prompts, and the organization catalog's listing under `/api/v1`.

use serde::{Deserialize, Serialize};

use crate::empty_as_none;
use crate::{BenchmarkDefinition, BenchmarkId, BenchmarkType, EvalSample, OrgId};

/// A catalog entry as `GET /benchmarks` lists it and `GET /benchmarks/{id}`
/// answers it. A corpus carries the `url` mgmt serves it at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    #[serde(flatten)]
    pub definition: BenchmarkDefinition,
    /// The organization the entry is private to; absent on a public one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<OrgId>,
    /// A text eval's prompts, on `GET /benchmarks/{id}` only. Never a
    /// vlm_eval's or an asr_eval's: those are the entry's corpus, downloaded
    /// from `GET /datasets/{id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<EvalSample>>,
}

/// `GET /benchmarks`'s query: the device catalog's filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogQuery {
    /// Either spelling [`BenchmarkType`] parses. An empty value is no filter.
    #[serde(
        rename = "type",
        default,
        deserialize_with = "benchmark_type_filter",
        skip_serializing_if = "Option::is_none"
    )]
    pub type_filter: Option<BenchmarkType>,
}

/// Read `type` as [`empty_as_none`] reads a filter, and refuse an unknown one by
/// listing the known types: [`BenchmarkType`]'s own error names only the value.
fn benchmark_type_filter<'de, D>(deserializer: D) -> Result<Option<BenchmarkType>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    empty_as_none(deserializer).map_err(|e: D::Error| {
        let known: Vec<String> = BenchmarkType::ALL.iter().map(ToString::to_string).collect();
        serde::de::Error::custom(format!("{e}; expected one of {}", known.join(", ")))
    })
}

/// The `/api/v1/orgs/{org_id}/benchmarks` listing body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OrgBenchmarks {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub org_id: OrgId,
    #[cfg_attr(feature = "schemars", schemars(with = "Vec<schema::OrgBenchmark>"))]
    pub benchmarks: Vec<CatalogEntry>,
    /// Ids kept under a key whose entry does not read, so it can be found and
    /// deleted.
    #[cfg_attr(feature = "schemars", schemars(with = "Vec<String>"))]
    pub unreadable_benchmark_ids: Vec<BenchmarkId>,
}

/// A catalog entry's shape for the app's schema bundle: a benchmark's
/// parameters stay open, as this crate gives them no schema.
#[cfg(feature = "schemars")]
pub(crate) mod schema {
    #![expect(
        dead_code,
        reason = "schema mirrors: only their schemas are read, so none is built"
    )]

    use schemars::JsonSchema;
    use serde_json::{Map, Value};

    #[derive(JsonSchema)]
    pub(crate) struct OrgBenchmark {
        pub(crate) benchmark_id: String,
        pub(crate) benchmark_type: String,
        /// Always present: every body this describes comes from an
        /// organization's catalog.
        pub(crate) org_id: String,
        #[serde(flatten)]
        pub(crate) parameters: Map<String, Value>,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A device reads back what mgmt serves: the flattened definition beside
    /// `org_id` and `samples`.
    #[test]
    fn an_entry_reads_back_as_served() -> anyhow::Result<()> {
        let served = json!({
            "benchmark_id": "eval_mmlu_pro",
            "benchmark_type": "eval",
            "parameter_eval_id": "mmlu_pro",
            "parameter_dataset_name": "edge_2026.03.1",
            "parameter_max_tokens": 256,
            "org_id": "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
            "samples": [{
                "id": "s1",
                "messages": [{"role": "user", "content": "2+2?"}],
            }],
        });
        let entry: CatalogEntry = serde_json::from_value(served.clone())?;
        assert!(entry.org_id.is_some());
        assert_eq!(serde_json::to_value(&entry)?, served);
        Ok(())
    }

    /// An asr_eval entry carries no samples: its sample count is its own
    /// `parameter_sample_count`, and its clips are its corpus.
    #[test]
    fn an_asr_entry_carries_no_samples() -> anyhow::Result<()> {
        let served = json!({
            "benchmark_id": "asr_eval_fleurs_nl_nl_voxtral",
            "benchmark_type": "asr_eval",
            "parameter_asr_task": "asr_fleurs_nl_nl",
            "parameter_dataset_version": "2026.10.1",
            "parameter_sample_count": 364,
            "parameter_prompt_profile": "voxtral",
            "parameter_max_tokens": 500,
            "parameter_temperature": 0.0,
        });
        let entry: CatalogEntry = serde_json::from_value(served.clone())?;
        assert!(entry.samples.is_none());
        assert_eq!(serde_json::to_value(&entry)?, served);
        Ok(())
    }
}
