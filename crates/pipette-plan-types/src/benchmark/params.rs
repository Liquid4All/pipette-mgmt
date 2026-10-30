//! [`BenchmarkParams`]: the parameters a benchmark is identified by, without
//! its id, and the [`BenchmarkEntry`] that serves them with it.

use serde::{Deserialize, Serialize};

use crate::benchmark::eval_id::EvalId;
use crate::benchmark::{
    AsrDecodeTokens, AsrEvalTask, AsrPromptProfile, AudioChannels, AudioDurationMillis,
    AudioSampleRate, BatchSize, BeamLengthPenalty, BeamRepetitionPenalty, BenchmarkDefinition,
    BenchmarkId, MinP, NumBeams, PresencePenalty, RepeatPenalty, SamplingSeed, SamplingTemperature,
    SeedRule, TopK, TopP, VlmEvalTask, VlmPromptProfile,
};
use crate::{BenchmarkType, NonNegativeCount, OrgId, PositiveCount};

/// A catalog entry reduced to its type and params. The pair is in the
/// submission's wire form, so a client copies both keys into its body as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkEntry {
    pub benchmark_id: BenchmarkId,
    /// Wire form: [`declared_benchmark_params`], and never absent.
    #[serde(flatten, with = "declared_benchmark_params::required")]
    pub benchmark_params: BenchmarkParams,
    /// The organization the entry is private to; absent on a public entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<OrgId>,
}

/// Every `parameter_*` field of a definition, without the prefix. The id and a
/// corpus `dataset` stay on [`BenchmarkDefinition`].
///
/// An eval's temperature is the one it runs at: the entry's, else the eval-id
/// rule's. Its other sampling knobs are `None` when unpinned, left to the client.
/// A vlm_eval's knobs, temperature included, are `None` when unpinned, which
/// is greedy with every other sampler neutral: an entry that pins none keeps
/// the params it had before the knobs existed. Its prompt profile is `None`
/// when it names none, so an entry without one keeps its params too.
/// An asr_eval's decoding knobs are `None` when unpinned (greedy) as a
/// vlm_eval's are; its prompt profile is required.
///
/// Each count has its definition's type, so a prompt may be empty (a decode
/// from no prompt, an image-only VL prompt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "benchmark_type", rename_all = "snake_case")]
pub enum BenchmarkParams {
    PrefillThroughput {
        prefill_tokens: NonNegativeCount,
        batch_size: BatchSize,
    },
    DecodeThroughput {
        prefill_tokens: NonNegativeCount,
        decode_tokens: PositiveCount,
        batch_size: BatchSize,
    },
    EndToEndLatency {
        prefill_tokens: NonNegativeCount,
        decode_tokens: PositiveCount,
        batch_size: BatchSize,
    },
    MaxMemoryUsage {
        prefill_tokens: NonNegativeCount,
    },
    Eval {
        eval_id: EvalId,
        dataset_name: String,
        max_tokens: PositiveCount,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_context: Option<PositiveCount>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcq_choices: Option<Vec<String>>,
        temperature: SamplingTemperature,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top_p: Option<TopP>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top_k: Option<TopK>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_p: Option<MinP>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed: Option<SamplingSeed>,
    },
    VlmEval {
        lmms_task: VlmEvalTask,
        dataset_version: String,
        sample_count: PositiveCount,
        max_tokens: PositiveCount,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<SamplingTemperature>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top_p: Option<TopP>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top_k: Option<TopK>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_p: Option<MinP>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presence_penalty: Option<PresencePenalty>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repeat_penalty: Option<RepeatPenalty>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed: Option<SamplingSeed>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed_rule: Option<SeedRule>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_profile: Option<VlmPromptProfile>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_tokens: Option<PositiveCount>,
    },
    AsrEval {
        asr_task: AsrEvalTask,
        dataset_version: String,
        sample_count: PositiveCount,
        prompt_profile: AsrPromptProfile,
        max_tokens: PositiveCount,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<SamplingTemperature>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top_p: Option<TopP>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top_k: Option<TopK>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_p: Option<MinP>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repeat_penalty: Option<RepeatPenalty>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed: Option<SamplingSeed>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed_rule: Option<SeedRule>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        num_beams: Option<NumBeams>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        beam_repetition_penalty: Option<BeamRepetitionPenalty>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        beam_length_penalty: Option<BeamLengthPenalty>,
    },
    VlThroughput {
        image_width: PositiveCount,
        image_height: PositiveCount,
        text_tokens: NonNegativeCount,
        decode_tokens: PositiveCount,
        num_images: NonNegativeCount,
    },
    VlMaxMemory {
        image_width: PositiveCount,
        image_height: PositiveCount,
        text_tokens: NonNegativeCount,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decode_tokens: Option<PositiveCount>,
        num_images: NonNegativeCount,
    },
    VlSpecDecode {
        draft_length: PositiveCount,
        max_tokens: PositiveCount,
        batch_size: BatchSize,
        temperature: SamplingTemperature,
    },
    AsrThroughput {
        audio_duration_ms: AudioDurationMillis,
        audio_sample_rate_hz: AudioSampleRate,
        audio_channels: AudioChannels,
        text_tokens: NonNegativeCount,
        decode_tokens: AsrDecodeTokens,
    },
    AsrMaxMemory {
        audio_duration_ms: AudioDurationMillis,
        audio_sample_rate_hz: AudioSampleRate,
        audio_channels: AudioChannels,
        text_tokens: NonNegativeCount,
        decode_tokens: AsrDecodeTokens,
    },
}

impl BenchmarkParams {
    pub fn benchmark_type(&self) -> BenchmarkType {
        match self {
            Self::PrefillThroughput { .. } => BenchmarkType::PrefillThroughput,
            Self::DecodeThroughput { .. } => BenchmarkType::DecodeThroughput,
            Self::EndToEndLatency { .. } => BenchmarkType::EndToEndLatency,
            Self::MaxMemoryUsage { .. } => BenchmarkType::MaxMemoryUsage,
            Self::Eval { .. } => BenchmarkType::Eval,
            Self::VlmEval { .. } => BenchmarkType::VlmEval,
            Self::AsrEval { .. } => BenchmarkType::AsrEval,
            Self::VlThroughput { .. } => BenchmarkType::VlThroughput,
            Self::VlMaxMemory { .. } => BenchmarkType::VlMaxMemory,
            Self::VlSpecDecode { .. } => BenchmarkType::VlSpecDecode,
            Self::AsrThroughput { .. } => BenchmarkType::AsrThroughput,
            Self::AsrMaxMemory { .. } => BenchmarkType::AsrMaxMemory,
        }
    }
}

/// Why a submission body's [`BenchmarkParams`] declaration is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BenchmarkParamsError {
    #[error("{present} requires {missing}")]
    Alone {
        present: &'static str,
        missing: &'static str,
    },
    #[error("invalid benchmark_type: {0}")]
    InvalidType(String),
    #[error("invalid benchmark_params: expected an object")]
    ParamsNotAnObject,
    #[error("invalid benchmark_params: its benchmark_type disagrees with the body's")]
    TagDisagrees,
    // A string: `serde_json::Error` is neither `Clone` nor `Eq`.
    #[error("invalid benchmark_params: {0}")]
    InvalidParams(String),
}

impl From<&BenchmarkDefinition> for BenchmarkParams {
    fn from(definition: &BenchmarkDefinition) -> Self {
        match definition {
            BenchmarkDefinition::PrefillThroughput(b) => Self::PrefillThroughput {
                prefill_tokens: b.parameter_prefill_tokens,
                batch_size: b.parameter_batch_size,
            },
            BenchmarkDefinition::DecodeThroughput(b) => Self::DecodeThroughput {
                prefill_tokens: b.parameter_prefill_tokens,
                decode_tokens: b.parameter_decode_tokens,
                batch_size: b.parameter_batch_size,
            },
            BenchmarkDefinition::EndToEndLatency(b) => Self::EndToEndLatency {
                prefill_tokens: b.parameter_prefill_tokens,
                decode_tokens: b.parameter_decode_tokens,
                batch_size: b.parameter_batch_size,
            },
            BenchmarkDefinition::MaxMemoryUsage(b) => Self::MaxMemoryUsage {
                prefill_tokens: b.parameter_prefill_tokens,
            },
            BenchmarkDefinition::Eval(b) => Self::Eval {
                eval_id: b.parameter_eval_id.clone(),
                dataset_name: b.parameter_dataset_name.clone(),
                max_tokens: b.parameter_max_tokens,
                max_context: b.parameter_max_context,
                mcq_choices: b.parameter_mcq_choices.clone(),
                // Every eval-id rule's temperature is in range (`eval_id.rs` tests it).
                temperature: b.sampling_temperature().unwrap_or_default(),
                top_p: b.parameter_top_p,
                top_k: b.parameter_top_k,
                min_p: b.parameter_min_p,
                seed: b.parameter_seed,
            },
            BenchmarkDefinition::VlmEval(b) => Self::VlmEval {
                lmms_task: b.parameter_lmms_task.clone(),
                dataset_version: b.parameter_dataset_version.clone(),
                sample_count: b.parameter_sample_count,
                max_tokens: b.parameter_max_tokens,
                temperature: b.parameter_temperature,
                top_p: b.parameter_top_p,
                top_k: b.parameter_top_k,
                min_p: b.parameter_min_p,
                presence_penalty: b.parameter_presence_penalty,
                repeat_penalty: b.parameter_repeat_penalty,
                seed: b.parameter_seed,
                seed_rule: b.parameter_seed_rule,
                prompt_profile: b.parameter_prompt_profile.clone(),
                image_tokens: b.parameter_image_tokens,
            },
            BenchmarkDefinition::AsrEval(b) => Self::AsrEval {
                asr_task: b.parameter_asr_task.clone(),
                dataset_version: b.parameter_dataset_version.clone(),
                sample_count: b.parameter_sample_count,
                prompt_profile: b.parameter_prompt_profile.clone(),
                max_tokens: b.parameter_max_tokens,
                temperature: b.parameter_temperature,
                top_p: b.parameter_top_p,
                top_k: b.parameter_top_k,
                min_p: b.parameter_min_p,
                repeat_penalty: b.parameter_repeat_penalty,
                seed: b.parameter_seed,
                seed_rule: b.parameter_seed_rule,
                num_beams: b.parameter_num_beams,
                beam_repetition_penalty: b.parameter_beam_repetition_penalty,
                beam_length_penalty: b.parameter_beam_length_penalty,
            },
            BenchmarkDefinition::VlThroughput(b) => Self::VlThroughput {
                image_width: b.parameter_image_width,
                image_height: b.parameter_image_height,
                text_tokens: b.parameter_text_tokens,
                decode_tokens: b.parameter_decode_tokens,
                num_images: b.parameter_num_images,
            },
            BenchmarkDefinition::VlMaxMemory(b) => Self::VlMaxMemory {
                image_width: b.parameter_image_width,
                image_height: b.parameter_image_height,
                text_tokens: b.parameter_text_tokens,
                decode_tokens: b.parameter_decode_tokens,
                num_images: b.parameter_num_images,
            },
            BenchmarkDefinition::VlSpecDecode(b) => Self::VlSpecDecode {
                draft_length: b.parameter_draft_length,
                max_tokens: b.parameter_max_tokens,
                batch_size: b.parameter_batch_size,
                temperature: b.parameter_temperature,
            },
            BenchmarkDefinition::AsrThroughput(b) => Self::AsrThroughput {
                audio_duration_ms: b.parameter_audio_duration_ms,
                audio_sample_rate_hz: b.parameter_audio_sample_rate_hz,
                audio_channels: b.parameter_audio_channels,
                text_tokens: b.parameter_text_tokens,
                decode_tokens: b.parameter_decode_tokens,
            },
            BenchmarkDefinition::AsrMaxMemory(b) => Self::AsrMaxMemory {
                audio_duration_ms: b.parameter_audio_duration_ms,
                audio_sample_rate_hz: b.parameter_audio_sample_rate_hz,
                audio_channels: b.parameter_audio_channels,
                text_tokens: b.parameter_text_tokens,
                decode_tokens: b.parameter_decode_tokens,
            },
        }
    }
}

/// The wire form of a declared [`BenchmarkParams`]: its tag lifted into the
/// body's own `benchmark_type`, beside the untagged `benchmark_params`. The
/// submission payload and [`BenchmarkEntry`] write it, and the server reads a
/// body with [`BenchmarkParams::from_submission_body`].
pub(crate) mod declared_benchmark_params {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    use crate::{BenchmarkParams, BenchmarkParamsError, BenchmarkType};

    const TYPE: &str = "benchmark_type";
    const PARAMS: &str = "benchmark_params";
    /// Keys the catalog never fills: each changes what a client executes, so
    /// a client that predates one did not apply it, and its result is refused
    /// rather than recorded as run with the entry's value.
    const MUST_DECLARE: &[&str] = &["image_tokens", "decode_tokens", "seed_rule"];

    impl BenchmarkParams {
        /// The pair a submission body declares; `None` when it sends neither.
        /// When `catalog` names the same type, a key the declaration leaves out
        /// takes the catalog's value, so a client that predates a field still
        /// declares the entry it ran, except a key in [`MUST_DECLARE`].
        pub fn from_submission_body(
            body: &Value,
            catalog: Option<&Self>,
        ) -> Result<Option<Self>, BenchmarkParamsError> {
            parse(body.get(TYPE), body.get(PARAMS), catalog)
        }
    }

    /// `null` counts as absent. An inner tag is tolerated only when it agrees.
    fn parse(
        benchmark_type: Option<&Value>,
        params: Option<&Value>,
        catalog: Option<&BenchmarkParams>,
    ) -> Result<Option<BenchmarkParams>, BenchmarkParamsError> {
        let benchmark_type = benchmark_type.filter(|v| !v.is_null());
        let params = params.filter(|v| !v.is_null());
        let (benchmark_type, params) = match (benchmark_type, params) {
            (None, None) => return Ok(None),
            (Some(t), Some(p)) => (t, p),
            (Some(_), None) => {
                return Err(BenchmarkParamsError::Alone {
                    present: TYPE,
                    missing: PARAMS,
                });
            }
            (None, Some(_)) => {
                return Err(BenchmarkParamsError::Alone {
                    present: PARAMS,
                    missing: TYPE,
                });
            }
        };
        let declared_type = BenchmarkType::deserialize(benchmark_type)
            .map_err(|e| BenchmarkParamsError::InvalidType(e.to_string()))?;
        let mut tagged = params.clone();
        let fields = tagged
            .as_object_mut()
            .ok_or(BenchmarkParamsError::ParamsNotAnObject)?;
        let inner = fields.insert(TYPE.into(), benchmark_type.clone());
        if inner.is_some_and(|inner| inner != *benchmark_type) {
            return Err(BenchmarkParamsError::TagDisagrees);
        }
        if let Some(catalog) = catalog.filter(|c| c.benchmark_type() == declared_type) {
            let Value::Object(known) = serde_json::to_value(catalog)
                .map_err(|e| BenchmarkParamsError::InvalidParams(e.to_string()))?
            else {
                return Err(BenchmarkParamsError::ParamsNotAnObject);
            };
            for (key, value) in known {
                if !MUST_DECLARE.contains(&key.as_str()) {
                    fields.entry(key).or_insert(value);
                }
            }
        }
        BenchmarkParams::deserialize(&tagged)
            .map(Some)
            .map_err(|e| BenchmarkParamsError::InvalidParams(e.to_string()))
    }

    /// A struct, not a map: under `flatten` it claims only these two keys.
    #[derive(Default, Serialize, Deserialize)]
    struct Wire {
        #[serde(skip_serializing_if = "Option::is_none")]
        benchmark_type: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        benchmark_params: Option<Value>,
    }

    impl Wire {
        fn of<E: serde::ser::Error>(params: &BenchmarkParams) -> Result<Self, E> {
            let Value::Object(mut fields) = serde_json::to_value(params).map_err(E::custom)? else {
                return Err(E::custom("BenchmarkParams is not an object"));
            };
            Ok(Self {
                benchmark_type: fields.remove(TYPE),
                benchmark_params: Some(Value::Object(fields)),
            })
        }
    }

    pub(crate) fn serialize<S: Serializer>(
        params: &Option<BenchmarkParams>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        params
            .as_ref()
            .map(Wire::of::<S::Error>)
            .transpose()?
            .unwrap_or_default()
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<BenchmarkParams>, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        parse(
            wire.benchmark_type.as_ref(),
            wire.benchmark_params.as_ref(),
            None,
        )
        .map_err(D::Error::custom)
    }

    /// The same pair where it is never absent: a catalog entry always has one.
    pub(crate) mod required {
        use serde::de::Error as _;
        use serde::{Deserializer, Serialize, Serializer};

        use crate::BenchmarkParams;

        pub(crate) fn serialize<S: Serializer>(
            params: &BenchmarkParams,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            super::Wire::of::<S::Error>(params)?.serialize(serializer)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<BenchmarkParams, D::Error> {
            super::deserialize(deserializer)?
                .ok_or_else(|| D::Error::custom("missing benchmark_type and benchmark_params"))
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::{Value, json};

    use super::*;
    use crate::dataset::mmspec_corpus;

    /// The definition holds the params' types, so a count the params could not
    /// hold is refused where the definition is read.
    #[rstest]
    #[case::empty_prompt(0, 32, true)]
    #[case::nothing_decoded(256, 0, false)]
    #[case::past_the_warehouse_column(256, u32::MAX, false)]
    fn a_prompt_may_be_empty_but_a_decode_may_not(
        #[case] prefill_tokens: u32,
        #[case] decode_tokens: u32,
        #[case] parses: bool,
    ) {
        let definition = serde_json::from_value::<BenchmarkDefinition>(json!({
            "benchmark_id": format!("decode_throughput_{prefill_tokens}_{decode_tokens}"),
            "benchmark_type": "decode_throughput",
            "parameter_prefill_tokens": prefill_tokens,
            "parameter_decode_tokens": decode_tokens,
        }));
        assert_eq!(definition.is_ok(), parses, "{definition:?}");
    }

    /// Every value is distinct, so a field read from its neighbor fails here.
    #[rstest]
    #[case::prefill(
        json!({"benchmark_id": "b", "benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 512, "parameter_batch_size": 4}),
        json!({"benchmark_type": "prefill_throughput", "prefill_tokens": 512, "batch_size": 4}),
    )]
    #[case::decode(
        json!({"benchmark_id": "b", "benchmark_type": "decode_throughput", "parameter_prefill_tokens": 512, "parameter_decode_tokens": 256, "parameter_batch_size": 8}),
        json!({"benchmark_type": "decode_throughput", "prefill_tokens": 512, "decode_tokens": 256, "batch_size": 8}),
    )]
    #[case::end_to_end(
        json!({"benchmark_id": "b", "benchmark_type": "end_to_end_latency", "parameter_prefill_tokens": 512, "parameter_decode_tokens": 128, "parameter_batch_size": 2}),
        json!({"benchmark_type": "end_to_end_latency", "prefill_tokens": 512, "decode_tokens": 128, "batch_size": 2}),
    )]
    #[case::max_memory(
        json!({"benchmark_id": "b", "benchmark_type": "max_memory_usage", "parameter_prefill_tokens": 1024}),
        json!({"benchmark_type": "max_memory_usage", "prefill_tokens": 1024}),
    )]
    #[case::eval(
        json!({"benchmark_id": "b", "benchmark_type": "eval", "parameter_eval_id": "mmlu_pro", "parameter_dataset_name": "edge_2026.03.1", "parameter_max_tokens": 256}),
        json!({"benchmark_type": "eval", "eval_id": "mmlu_pro", "dataset_name": "edge_2026.03.1", "max_tokens": 256, "temperature": 0.0}),
    )]
    // Unpinned, the temperature is the eval-id rule's: what every client runs.
    #[case::eval_rule_temperature(
        json!({"benchmark_id": "b", "benchmark_type": "eval", "parameter_eval_id": "gpqa_diamond", "parameter_dataset_name": "2026.06.1", "parameter_max_tokens": 8192}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6}),
    )]
    #[case::eval_sampling(
        json!({"benchmark_id": "b", "benchmark_type": "eval", "parameter_eval_id": "mmlu_pro", "parameter_dataset_name": "edge_2026.03.1", "parameter_max_tokens": 256, "parameter_mcq_choices": ["A", "B"], "parameter_temperature": 1.0, "parameter_top_p": 0.95, "parameter_top_k": 64, "parameter_min_p": 0.05, "parameter_seed": 7}),
        json!({"benchmark_type": "eval", "eval_id": "mmlu_pro", "dataset_name": "edge_2026.03.1", "max_tokens": 256, "mcq_choices": ["A", "B"], "temperature": 1.0, "top_p": 0.95, "top_k": 64, "min_p": 0.05, "seed": 7}),
    )]
    #[case::eval_max_context(
        json!({"benchmark_id": "b", "benchmark_type": "eval", "parameter_eval_id": "math_500", "parameter_dataset_name": "2026.10.1", "parameter_max_tokens": 32768, "parameter_max_context": 32768}),
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 32768, "temperature": 0.6}),
    )]
    // Unpinned, a vlm_eval's params carry no knob at all, temperature included.
    #[case::vlm_eval(
        json!({"benchmark_id": "b", "benchmark_type": "vlm_eval", "parameter_lmms_task": "mmstar", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 1500, "parameter_max_tokens": 8192}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192}),
    )]
    #[case::vlm_eval_sampling(
        json!({"benchmark_id": "b", "benchmark_type": "vlm_eval", "parameter_lmms_task": "mmstar", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 1500, "parameter_max_tokens": 8192, "parameter_temperature": 0.7, "parameter_top_p": 0.8, "parameter_top_k": 20, "parameter_min_p": 0.05, "parameter_presence_penalty": 1.5, "parameter_repeat_penalty": 1.05, "parameter_seed": 7}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "temperature": 0.7, "top_p": 0.8, "top_k": 20, "min_p": 0.05, "presence_penalty": 1.5, "repeat_penalty": 1.05, "seed": 7}),
    )]
    #[case::asr_eval(
        json!({"benchmark_id": "b", "benchmark_type": "asr_eval", "parameter_asr_task": "asr_esb_librispeech_test_clean", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 2620, "parameter_prompt_profile": "minicpm_o", "parameter_max_tokens": 128, "parameter_num_beams": 3, "parameter_beam_repetition_penalty": 1.2, "parameter_beam_length_penalty": 1.0}),
        json!({"benchmark_type": "asr_eval", "asr_task": "asr_esb_librispeech_test_clean", "dataset_version": "2026.10.1", "sample_count": 2620, "prompt_profile": "minicpm_o", "max_tokens": 128, "num_beams": 3, "beam_repetition_penalty": 1.2, "beam_length_penalty": 1.0}),
    )]
    #[case::vlm_eval_seed_rule(
        json!({"benchmark_id": "b", "benchmark_type": "vlm_eval", "parameter_lmms_task": "mmstar", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 1500, "parameter_max_tokens": 8192, "parameter_temperature": 0.7, "parameter_seed": 0, "parameter_seed_rule": "per_sample_sha256"}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "temperature": 0.7, "seed": 0, "seed_rule": "per_sample_sha256"}),
    )]
    #[case::vlm_eval_prompt_profile(
        json!({"benchmark_id": "b", "benchmark_type": "vlm_eval", "parameter_lmms_task": "mmstar", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 1500, "parameter_max_tokens": 8192, "parameter_prompt_profile": "internvl"}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "prompt_profile": "internvl"}),
    )]
    #[case::vlm_eval_image_tokens(
        json!({"benchmark_id": "b", "benchmark_type": "vlm_eval", "parameter_lmms_task": "pope", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 5127, "parameter_max_tokens": 32768, "parameter_image_tokens": 1120}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "image_tokens": 1120}),
    )]
    #[case::vl_throughput(
        json!({"benchmark_id": "b", "benchmark_type": "vl_throughput", "parameter_image_width": 224, "parameter_image_height": 336, "parameter_text_tokens": 8, "parameter_decode_tokens": 64, "parameter_num_images": 3}),
        json!({"benchmark_type": "vl_throughput", "image_width": 224, "image_height": 336, "text_tokens": 8, "decode_tokens": 64, "num_images": 3}),
    )]
    #[case::vl_max_memory(
        json!({"benchmark_id": "b", "benchmark_type": "vl_max_memory", "parameter_image_width": 224, "parameter_image_height": 336}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 224, "image_height": 336, "text_tokens": 0, "num_images": 1}),
    )]
    #[case::vl_max_memory_decode(
        json!({"benchmark_id": "b", "benchmark_type": "vl_max_memory", "parameter_image_width": 224, "parameter_image_height": 336, "parameter_text_tokens": 8, "parameter_decode_tokens": 256, "parameter_num_images": 3}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 224, "image_height": 336, "text_tokens": 8, "decode_tokens": 256, "num_images": 3}),
    )]
    #[case::vl_spec_decode(
        json!({"benchmark_id": "b", "benchmark_type": "vl_spec_decode", "parameter_draft_length": 7, "parameter_max_tokens": 2048, "parameter_batch_size": 8, "parameter_temperature": 0.7, "dataset": mmspec_corpus()}),
        json!({"benchmark_type": "vl_spec_decode", "draft_length": 7, "max_tokens": 2048, "batch_size": 8, "temperature": 0.7}),
    )]
    #[case::asr_throughput(
        json!({"benchmark_id": "b", "benchmark_type": "asr_throughput", "parameter_audio_duration_ms": 9400, "parameter_audio_sample_rate_hz": 16000, "parameter_audio_channels": 1, "parameter_text_tokens": 1024, "parameter_decode_tokens": 128}),
        json!({"benchmark_type": "asr_throughput", "audio_duration_ms": 9400, "audio_sample_rate_hz": 16000, "audio_channels": 1, "text_tokens": 1024, "decode_tokens": 128}),
    )]
    // Text left out is none: the clip and the ask alone.
    #[case::asr_throughput_no_text(
        json!({"benchmark_id": "b", "benchmark_type": "asr_throughput", "parameter_audio_duration_ms": 9400, "parameter_audio_sample_rate_hz": 16000, "parameter_audio_channels": 1, "parameter_decode_tokens": 128}),
        json!({"benchmark_type": "asr_throughput", "audio_duration_ms": 9400, "audio_sample_rate_hz": 16000, "audio_channels": 1, "text_tokens": 0, "decode_tokens": 128}),
    )]
    #[case::asr_max_memory(
        json!({"benchmark_id": "b", "benchmark_type": "asr_max_memory", "parameter_audio_duration_ms": 9400, "parameter_audio_sample_rate_hz": 48000, "parameter_audio_channels": 2, "parameter_text_tokens": 1024, "parameter_decode_tokens": 256}),
        json!({"benchmark_type": "asr_max_memory", "audio_duration_ms": 9400, "audio_sample_rate_hz": 48000, "audio_channels": 2, "text_tokens": 1024, "decode_tokens": 256}),
    )]
    #[case::asr_max_memory_no_text(
        json!({"benchmark_id": "b", "benchmark_type": "asr_max_memory", "parameter_audio_duration_ms": 9400, "parameter_audio_sample_rate_hz": 48000, "parameter_audio_channels": 2, "parameter_decode_tokens": 256}),
        json!({"benchmark_type": "asr_max_memory", "audio_duration_ms": 9400, "audio_sample_rate_hz": 48000, "audio_channels": 2, "text_tokens": 0, "decode_tokens": 256}),
    )]
    fn a_definition_maps_to_its_params(
        #[case] definition: Value,
        #[case] expected: Value,
    ) -> anyhow::Result<()> {
        let definition: BenchmarkDefinition = serde_json::from_value(definition)?;
        let params = BenchmarkParams::from(&definition);
        assert_eq!(serde_json::to_value(&params)?, expected);
        assert_eq!(params.benchmark_type(), definition.benchmark_type());
        Ok(())
    }

    /// A declaration read over the catalog agrees when every field it states
    /// matches.
    #[rstest]
    #[case::narrow_eval(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192}),
        true
    )]
    #[case::same_temperature(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        true
    )]
    #[case::other_temperature(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6}),
        false
    )]
    #[case::other_required(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 4096, "temperature": 1.0}),
        false
    )]
    #[case::narrow_vl_max_memory(
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "num_images": 5}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256}),
        true
    )]
    #[case::other_image_count(
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "num_images": 5}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "num_images": 1}),
        false
    )]
    // A peak run at another decode count is another workload.
    #[case::other_asr_max_memory_decode(
        json!({"benchmark_type": "asr_max_memory", "audio_duration_ms": 2500, "audio_sample_rate_hz": 16000, "audio_channels": 1, "text_tokens": 0, "decode_tokens": 256}),
        json!({"benchmark_type": "asr_max_memory", "audio_duration_ms": 2500, "audio_sample_rate_hz": 16000, "audio_channels": 1, "text_tokens": 0, "decode_tokens": 2}),
        false
    )]
    // What a client built before the decode count declares, on an entry
    // without one.
    #[case::legacy_vl_max_memory(
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "num_images": 5}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "num_images": 5}),
        true
    )]
    // The same client on an entry with one decoded a single token: its result
    // is not the entry's, so the catalog does not fill the count in.
    #[case::decode_tokens_left_out(
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "decode_tokens": 256, "num_images": 5}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "num_images": 5}),
        false
    )]
    #[case::decode_tokens_declared(
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "decode_tokens": 256, "num_images": 5}),
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 1024, "decode_tokens": 256, "num_images": 5}),
        true
    )]
    #[case::narrow_vl_spec_decode(
        json!({"benchmark_type": "vl_spec_decode", "draft_length": 7, "max_tokens": 2048, "batch_size": 1, "temperature": 0.7}),
        json!({"benchmark_type": "vl_spec_decode", "draft_length": 7, "max_tokens": 2048, "batch_size": 1}),
        true
    )]
    #[case::other_kind(
        json!({"benchmark_type": "max_memory_usage", "prefill_tokens": 256}),
        json!({"benchmark_type": "prefill_throughput", "prefill_tokens": 256, "batch_size": 1}),
        false
    )]
    // The comparison is typed, so a whole temperature may be sent as an integer.
    #[case::integer_temperature(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1}),
        true
    )]
    // A null is stated, not left out: it declares the knob unpinned.
    #[case::null_on_a_pinned_knob(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6, "top_p": 0.95}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "top_p": null}),
        false
    )]
    #[case::null_on_an_unpinned_knob(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "top_p": null}),
        true
    )]
    #[case::narrow_eval_every_knob(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "mcq_choices": ["A", "B"], "temperature": 1.0, "top_p": 0.95, "top_k": 64, "min_p": 0.05, "seed": 7}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192}),
        true
    )]
    #[case::other_seed(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6, "seed": 7}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "seed": 8}),
        false
    )]
    #[case::reordered_mcq_choices(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6, "mcq_choices": ["A", "B"]}),
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "mcq_choices": ["B", "A"]}),
        false
    )]
    #[case::narrow_vl_throughput(
        json!({"benchmark_type": "vl_throughput", "image_width": 224, "image_height": 224, "text_tokens": 0, "decode_tokens": 64, "num_images": 3}),
        json!({"benchmark_type": "vl_throughput", "image_width": 224, "image_height": 224, "text_tokens": 0, "decode_tokens": 64}),
        true
    )]
    #[case::other_vl_throughput_image_count(
        json!({"benchmark_type": "vl_throughput", "image_width": 224, "image_height": 224, "text_tokens": 0, "decode_tokens": 64, "num_images": 3}),
        json!({"benchmark_type": "vl_throughput", "image_width": 224, "image_height": 224, "text_tokens": 0, "decode_tokens": 64, "num_images": 1}),
        false
    )]
    #[case::narrow_vlm_eval_every_knob(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "temperature": 0.7, "top_p": 0.8, "top_k": 20, "min_p": 0.05, "presence_penalty": 1.5, "repeat_penalty": 1.05, "seed": 7}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192}),
        true
    )]
    #[case::other_vlm_eval_penalty(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "presence_penalty": 1.5}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "presence_penalty": 0.0}),
        false
    )]
    // A client built before the profile declares none; the catalog's fills it.
    #[case::narrow_vlm_eval_prompt_profile(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "prompt_profile": "internvl"}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192}),
        true
    )]
    #[case::other_vlm_eval_prompt_profile(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "prompt_profile": "internvl"}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "prompt_profile": "lfm_vl"}),
        false
    )]
    // A client built before the image budget did not apply it: its result is
    // not the entry's, so the catalog does not fill the budget in.
    #[case::image_tokens_left_out(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "image_tokens": 1120}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768}),
        false
    )]
    #[case::image_tokens_declared(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "image_tokens": 1120}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "image_tokens": 1120}),
        true
    )]
    // A client built before the seed rule drew with the repeat offset: its
    // result is not the entry's, so the catalog does not fill the rule in.
    #[case::seed_rule_left_out(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "temperature": 0.7, "seed": 0, "seed_rule": "per_sample_sha256"}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "temperature": 0.7, "seed": 0}),
        false
    )]
    #[case::seed_rule_declared(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "temperature": 0.7, "seed": 0, "seed_rule": "per_sample_sha256"}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "temperature": 0.7, "seed": 0, "seed_rule": "per_sample_sha256"}),
        true
    )]
    // An entry that names no rule still takes a declaration that names none.
    #[case::seed_without_a_rule(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "temperature": 0.7, "seed": 0}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "pope", "dataset_version": "2026.10.1", "sample_count": 5127, "max_tokens": 32768, "temperature": 0.7, "seed": 0}),
        true
    )]
    #[case::asr_seed_rule_left_out(
        json!({"benchmark_type": "asr_eval", "asr_task": "asr_fleurs_nl_nl", "dataset_version": "2026.10.1", "sample_count": 364, "prompt_profile": "gemma4", "max_tokens": 512, "temperature": 1.0, "seed": 0, "seed_rule": "per_sample_sha256"}),
        json!({"benchmark_type": "asr_eval", "asr_task": "asr_fleurs_nl_nl", "dataset_version": "2026.10.1", "sample_count": 364, "prompt_profile": "gemma4", "max_tokens": 512, "temperature": 1.0, "seed": 0}),
        false
    )]
    // An entry without a profile asks for the dataset's own prompt, not a gap
    // the client fills.
    #[case::prompt_profile_on_an_unprofiled_entry(
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192}),
        json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "prompt_profile": "internvl"}),
        false
    )]
    // A client built before the field declares none; the catalog's fills it.
    #[case::narrow_max_context(
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 32768, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768}),
        true
    )]
    #[case::same_max_context(
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 32768, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 32768}),
        true
    )]
    #[case::other_max_context(
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 32768, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 16384}),
        false
    )]
    // A bound is part of the measurement, not a knob left to the client.
    #[case::max_context_on_an_unbounded_entry(
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "max_context": 32768}),
        false
    )]
    // Unknown keys are ignored so a newer client's field still agrees; a
    // misspelled one is then as if left out.
    #[case::misspelled_key(
        json!({"benchmark_type": "prefill_throughput", "prefill_tokens": 256, "batch_size": 4}),
        json!({"benchmark_type": "prefill_throughput", "prefill_tokens": 256, "batchsize": 1}),
        true
    )]
    fn a_declaration_agrees_on_what_it_states(
        #[case] catalog: Value,
        #[case] declared: Value,
        #[case] agrees: bool,
    ) -> anyhow::Result<()> {
        let catalog: BenchmarkParams = serde_json::from_value(catalog)?;
        let body =
            json!({"benchmark_type": declared["benchmark_type"], "benchmark_params": declared});
        let read = BenchmarkParams::from_submission_body(&body, Some(&catalog))?;
        assert_eq!(read.as_ref() == Some(&catalog), agrees, "{read:?}");
        Ok(())
    }

    /// Without a catalog to read over, a declaration must carry every field.
    #[rstest]
    #[case::eval_temperature(
        json!({"benchmark_type": "eval", "benchmark_params": {"eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192}}),
        "missing field `temperature`"
    )]
    #[case::vl_max_memory_text(
        json!({"benchmark_type": "vl_max_memory", "benchmark_params": {"image_width": 256, "image_height": 256}}),
        "missing field `text_tokens`"
    )]
    #[case::vl_max_memory_images(
        json!({"benchmark_type": "vl_max_memory", "benchmark_params": {"image_width": 256, "image_height": 256, "text_tokens": 0}}),
        "missing field `num_images`"
    )]
    #[case::vl_throughput_images(
        json!({"benchmark_type": "vl_throughput", "benchmark_params": {"image_width": 224, "image_height": 224, "text_tokens": 0, "decode_tokens": 64}}),
        "missing field `num_images`"
    )]
    #[case::vl_spec_decode_temperature(
        json!({"benchmark_type": "vl_spec_decode", "benchmark_params": {"draft_length": 8, "max_tokens": 2048, "batch_size": 1}}),
        "missing field `temperature`"
    )]
    fn a_declaration_alone_carries_every_field(#[case] body: Value, #[case] reason: &str) {
        let read = BenchmarkParams::from_submission_body(&body, None);
        assert!(
            read.as_ref().is_err_and(|e| e.to_string().contains(reason)),
            "{read:?}"
        );
    }

    /// Reading over a catalog excuses nothing a declaration states, and fills
    /// nothing from another type's.
    #[rstest]
    #[case::temperature_out_of_range(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}),
        json!({"benchmark_type": "eval", "benchmark_params": {"temperature": 2.5}}),
        "SamplingTemperature is too big"
    )]
    #[case::mcq_choices_not_a_list(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "benchmark_params": {"mcq_choices": "AB"}}),
        "expected a sequence"
    )]
    #[case::null_temperature(
        json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 0.6}),
        json!({"benchmark_type": "eval", "benchmark_params": {"temperature": null}}),
        "invalid type: null"
    )]
    #[case::null_on_a_required_field(
        json!({"benchmark_type": "vl_max_memory", "image_width": 256, "image_height": 256, "text_tokens": 0, "num_images": 1}),
        json!({"benchmark_type": "vl_max_memory", "benchmark_params": {"num_images": null}}),
        "invalid type: null"
    )]
    #[case::no_fill_across_types(
        json!({"benchmark_type": "vl_throughput", "image_width": 256, "image_height": 256, "text_tokens": 0, "decode_tokens": 64, "num_images": 1}),
        json!({"benchmark_type": "vl_max_memory", "benchmark_params": {"image_width": 256, "image_height": 256}}),
        "missing field `text_tokens`"
    )]
    #[case::decode_tokens_never_filled(
        json!({"benchmark_type": "vl_throughput", "image_width": 224, "image_height": 224, "text_tokens": 0, "decode_tokens": 64, "num_images": 1}),
        json!({"benchmark_type": "vl_throughput", "benchmark_params": {"image_width": 224, "image_height": 224, "text_tokens": 0, "num_images": 1}}),
        "missing field `decode_tokens`"
    )]
    fn a_declaration_is_refused_over_the_catalog(
        #[case] catalog: Value,
        #[case] body: Value,
        #[case] reason: &str,
    ) -> anyhow::Result<()> {
        let catalog: BenchmarkParams = serde_json::from_value(catalog)?;
        let read = BenchmarkParams::from_submission_body(&body, Some(&catalog));
        assert!(
            read.as_ref().is_err_and(|e| e.to_string().contains(reason)),
            "{read:?}"
        );
        Ok(())
    }

    /// An entry's pair is what a submission body declares, read by the same parse.
    #[rstest]
    #[case::public(None)]
    #[case::private(Some("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"))]
    fn an_entry_carries_the_pair_a_submission_declares(
        #[case] org_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let entry = BenchmarkEntry {
            benchmark_id: BenchmarkId::try_new("decode_throughput_512_256_bs4")?,
            benchmark_params: BenchmarkParams::DecodeThroughput {
                prefill_tokens: NonNegativeCount::try_new(512)?,
                decode_tokens: PositiveCount::try_new(256)?,
                batch_size: BatchSize::try_new(4)?,
            },
            org_id: org_id.map(|id| OrgId::try_new(id.to_owned())).transpose()?,
        };
        let mut expected = json!({
            "benchmark_id": "decode_throughput_512_256_bs4",
            "benchmark_type": "decode_throughput",
            "benchmark_params": {"prefill_tokens": 512, "decode_tokens": 256, "batch_size": 4},
        });
        if let Some(org_id) = org_id {
            expected["org_id"] = org_id.into();
        }
        let json = serde_json::to_value(&entry)?;
        assert_eq!(json, expected);
        assert_eq!(
            BenchmarkParams::from_submission_body(&json, None)?,
            Some(entry.benchmark_params.clone())
        );
        assert_eq!(serde_json::from_value::<BenchmarkEntry>(json)?, entry);
        Ok(())
    }

    #[rstest]
    #[case::no_pair(json!({"benchmark_id": "b"}), "missing benchmark_type and benchmark_params")]
    #[case::type_alone(
        json!({"benchmark_id": "b", "benchmark_type": "max_memory_usage"}),
        "benchmark_type requires benchmark_params"
    )]
    fn an_entry_without_its_pair_is_refused(#[case] json: Value, #[case] reason: &str) {
        let refused = serde_json::from_value::<BenchmarkEntry>(json);
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.to_string().contains(reason)),
            "{refused:?}"
        );
    }
}
