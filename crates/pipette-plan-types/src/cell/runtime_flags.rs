//! Typed runtime flags for a plan's cells: one [`RuntimeFlags`] variant per
//! runnable `(benchmark, runtime_type, model)` cell, authored flat as
//! [`RuntimeFlagRef`]. Knob values are logical, not tool argv; clients map them.

use serde::{Deserialize, Serialize};

use crate::{
    BenchmarkType, DockerIpcMode, DockerShmSize, EnvForward, LlamacppFlashAttention,
    LlamacppLoadMode, Model, ModelType, OpenvinoDevice, OpenvinoGenerateHint, PositiveCount,
    Runtime, RuntimeType, TrtEdgellmPrecision, VllmDtype, VllmRepetitionDetection,
};

/// Why a [`RuntimeFlags`] could not be built, from a flat [`RuntimeFlagRef`] or from a
/// client's `--runtime-flags`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RuntimeFlagsError {
    /// An entry for another cell, converted to one cell's flags type.
    #[error(
        "runtime flags for {benchmark} \u{d7} {runtime:?} \u{d7} {model:?} are not a {expected} cell's"
    )]
    NotThisCell {
        expected: &'static str,
        benchmark: BenchmarkType,
        runtime: RuntimeType,
        model: ModelType,
    },
    /// No variant for this `(benchmark, runtime, model)` triple.
    #[error("no runtime flags defined for {benchmark} \u{d7} {runtime:?} \u{d7} {model:?}")]
    NoSuchCombination {
        benchmark: BenchmarkType,
        runtime: RuntimeType,
        model: ModelType,
    },
    /// A typed knob was set that this cell doesn't accept.
    #[error("knob `{knob}` is not accepted by {benchmark} \u{d7} {runtime:?} \u{d7} {model:?}")]
    KnobNotAllowed {
        knob: &'static str,
        benchmark: BenchmarkType,
        runtime: RuntimeType,
        model: ModelType,
    },
    /// A `raw` entry uses the spelling of a knob this cell types.
    #[error("raw flag {flag:?} aliases a typed knob for this cell; set the typed field instead")]
    RawAliasesTypedKnob { flag: String },
    /// A `raw` entry uses a flag the benchmark/tool fixes internally.
    #[error("raw flag {flag:?} is reserved by the benchmark/tool for this cell")]
    RawReservedFlag { flag: String },
    /// A vLLM vlm_eval or asr_eval cell's `raw` entry is not one of the
    /// speed, memory and logging flags that cell may set
    /// ([`RuntimeFlags::raw_allowlist`]).
    #[error(
        "raw flag {flag:?} is not one a {benchmark} cell on vLLM may set: its catalog entry owns \
         everything that can change an answer, and `raw` takes only speed, memory and logging \
         flags, by their full names"
    )]
    RawNotAllowed {
        flag: String,
        benchmark: crate::BenchmarkType,
    },
    /// A vLLM vlm_eval or asr_eval cell's env forward is not one that cell
    /// may set ([`RuntimeFlags::validate_envs`]): vLLM reads `VLLM_*` variables
    /// that redirect the weights, load plugins or change kernels and sampling.
    #[error(
        "env forward {name:?} is not one a {benchmark} cell on vLLM may set: its catalog entry \
         owns everything that can change an answer, and `envs` takes only device selection, \
         threads, the collective library's settings, logging and cache locations"
    )]
    EnvNotAllowed {
        name: String,
        benchmark: crate::BenchmarkType,
    },
    /// An env forward sets a knob the benchmark/tool fixes internally (the
    /// `LLAMA_ARG_*` spelling of a reserved flag).
    #[error("env forward {name:?} is reserved by the benchmark/tool for this cell")]
    ReservedEnvForward { name: String },

    /// A vlm_eval or asr_eval cell's `envs` set a llama.cpp option variable:
    /// llama-server reads it before its argv, so it could change a setting the
    /// entry's recipe owns where no recorded flag shows it.
    #[error(
        "env {name:?} sets a llama.cpp option on a {benchmark} cell; its catalog entry owns the \
         decode, and the typed knobs and `raw` set the rest"
    )]
    LlamaArgEnv {
        benchmark: crate::BenchmarkType,
        name: String,
    },
    /// A `raw` entry configures speculation on a run with speculation off: it
    /// would make the baseline row speculative.
    #[error(
        "raw flag {flag:?} configures speculation, but speculation is off for this \
         run (a model with no drafter, or speculative_enabled = false)"
    )]
    RawSpeculativeFlagWithSpeculationOff { flag: String },
    /// `speculative_enabled = true` on a model with no drafter to enable.
    #[error(
        "speculative_enabled = true, but a {model} model has no drafter; use a \
         speculative model type, or leave the flag unset"
    )]
    SpeculationWithoutDrafter { model: ModelType },
    /// A `repetition_detection` pattern range vLLM would refuse.
    #[error("repetition_detection: min_pattern_size {min} exceeds max_pattern_size {max}")]
    RepetitionPatternRange {
        min: PositiveCount,
        max: PositiveCount,
    },
    /// A `repetition_detection` repeat count vLLM would refuse.
    #[error(
        "repetition_detection: min_count must be at least 2, got {0}: a pattern repeats once it \
         comes twice"
    )]
    RepetitionMinCount(PositiveCount),
    // A string: `serde_json::Error` is neither `Clone` nor `Eq`.
    #[error("parsing runtime flags JSON: {0}")]
    Json(String),
    #[error(
        "runtime flags must be a JSON object of knobs, e.g. {{\"threads\":4}} \
         (the one-element array is no longer accepted)"
    )]
    NotAnObject,
    #[error(
        "runtime flags must not carry `{key}`: the cell comes from --benchmark, --runtime \
         and --model, which are parsed first"
    )]
    AxisNotAccepted { key: &'static str },
}

impl From<serde_json::Error> for RuntimeFlagsError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

// The payloads' schemas are `inline`, so each variant's arm is one object rather
// than a `$ref` beside the tag.

/// The flags of a `prefill_throughput` cell on llama.cpp's CLI tools with a GGUF text model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct PrefillThroughputLlamacppCliStockToolsGgufText {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of a `decode_throughput` cell on llama.cpp's CLI tools with a GGUF text model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct DecodeThroughputLlamacppCliStockToolsGgufText {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of a `max_memory_usage` cell on llama.cpp's CLI tools with a GGUF text model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct MaxMemoryUsageLlamacppCliStockToolsGgufText {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of an `end_to_end_latency` cell on llama.cpp's CLI tools with a GGUF text model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct EndToEndLatencyLlamacppCliStockToolsGgufText {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of an `eval` cell on llama.cpp's CLI tools with a GGUF text model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct EvalLlamacppCliStockToolsGgufText {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    /// llama-server slot fan-out (`--parallel N`): N samples run concurrently
    /// against the one server. Unset or `1` is the serial run and emits no flag.
    ///
    /// llama-server divides `--ctx-size` across slots. An explicit `ctx_size`
    /// is the total, so size it as `N x (longest prompt + max_tokens)`; unset,
    /// the per-cell default (`8192 + max_tokens`, one slot's) is multiplied by N.
    ///
    /// Only zero is rejected: a ceiling would have to guess the host's VRAM,
    /// which this crate cannot see. llama-server fails loudly if it cannot
    /// allocate.
    pub parallel: Option<PositiveCount>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of a `vl_throughput` cell on llama.cpp's CLI tools with a GGUF vision model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct VlThroughputLlamacppCliStockToolsGgufVision {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of an `asr_throughput` cell on llama.cpp's CLI tools with a GGUF
/// multimodal model (an audio encoder in its projector).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct AsrThroughputLlamacppCliStockToolsGgufVision {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of a `vlm_eval` cell on llama.cpp's CLI tools with a GGUF vision model: the
/// `vl_throughput` knobs plus `parallel`. Whether the model thinks is not a runtime flag:
/// it is the cell's model flags' `enable_thinking`, as on a text eval.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct VlmEvalLlamacppCliStockToolsGgufVision {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    /// llama-server slot fan-out (`--parallel N`), as on
    /// [`EvalLlamacppCliStockToolsGgufText::parallel`]. An explicit `ctx_size` is the TOTAL,
    /// divided across slots; omitted, the client calibrates one sample's budget from the
    /// dataset and multiplies it by `N`.
    pub parallel: Option<PositiveCount>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of an `asr_eval` cell on llama.cpp's CLI tools with a GGUF vision (audio)
/// model: the `vlm_eval` knobs without `reasoning`, since an ASR model never thinks and the
/// client runs every one with thinking off.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct AsrEvalLlamacppCliStockToolsGgufVision {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    /// llama-server slot fan-out (`--parallel N`), as on the vlm_eval cell. A beam-search
    /// entry runs one slot (the `--beam-search` of pipette's ASR build of llama.cpp needs
    /// `--parallel 1`).
    pub parallel: Option<PositiveCount>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of a `vl_max_memory` cell on llama.cpp's CLI tools with a GGUF vision model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct VlMaxMemoryLlamacppCliStockToolsGgufVision {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of an `asr_max_memory` cell on llama.cpp's CLI tools with a GGUF
/// multimodal model (an audio encoder in its projector).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct AsrMaxMemoryLlamacppCliStockToolsGgufVision {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
}

/// The flags of a `vl_spec_decode` cell on llama.cpp's CLI tools, with a GGUF vision model or a
/// speculative one: both corpus variants carry these, and differ only in the model axis.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct VlSpecDecodeLlamacppCliStockTools {
    pub threads: Option<u32>,
    pub number_gpu_layers: Option<u32>,
    pub mmap: Option<bool>,
    pub load_mode: Option<LlamacppLoadMode>,
    pub flash_attention: Option<LlamacppFlashAttention>,
    pub ctx_size: Option<u32>,
    pub no_cache: Option<bool>,
    pub envs: Vec<EnvForward>,
    pub raw: Vec<String>,
    pub speculative_enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct PrefillThroughputUvOpenvinoOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct DecodeThroughputUvOpenvinoOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct EndToEndLatencyUvOpenvinoOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct MaxMemoryUsageUvOpenvinoOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct PrefillThroughputOvmsOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
    pub prefix_caching: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct DecodeThroughputOvmsOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
    pub prefix_caching: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct EndToEndLatencyOvmsOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
    pub prefix_caching: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct MaxMemoryUsageOvmsOpenvino {
    pub device: Option<OpenvinoDevice>,
    pub max_prompt_len: Option<PositiveCount>,
    pub min_response_len: Option<PositiveCount>,
    pub generate_hint: Option<OpenvinoGenerateHint>,
    pub prefix_caching: Option<bool>,
}

/// The flags of the `vl_throughput` and `vl_max_memory` cells on TensorRT Edge-LLM with a
/// GGUF vision model: both carry these, and differ only in the benchmark axis.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema), schemars(inline))]
pub struct TrtEdgellmGgufVision {
    /// The language model's engine precision; unset, the server's default.
    pub precision: Option<TrtEdgellmPrecision>,
    pub envs: Vec<EnvForward>,
    /// Server flags, e.g. the robo state and embodiment of a VLA run.
    pub raw: Vec<String>,
}

/// Converts the flat form to the typed struct for one cell, refusing an entry for any other
/// cell.
macro_rules! cell_flags {
    ($flags:ident: $($variant:ident),+) => {
        impl TryFrom<RuntimeFlagRef> for $flags {
            type Error = RuntimeFlagsError;

            fn try_from(r: RuntimeFlagRef) -> Result<Self, Self::Error> {
                let (benchmark, runtime, model) = (r.benchmark_type, r.runtime_type, r.model_type);
                match RuntimeFlags::try_from(r)? {
                    $(RuntimeFlags::$variant(flags))|+ => Ok(flags),
                    _ => Err(RuntimeFlagsError::NotThisCell {
                        expected: stringify!($flags),
                        benchmark,
                        runtime,
                        model,
                    }),
                }
            }
        }
    };
}

cell_flags!(PrefillThroughputLlamacppCliStockToolsGgufText: PrefillThroughputLlamacppCliStockToolsGgufText);
cell_flags!(DecodeThroughputLlamacppCliStockToolsGgufText: DecodeThroughputLlamacppCliStockToolsGgufText);
cell_flags!(MaxMemoryUsageLlamacppCliStockToolsGgufText: MaxMemoryUsageLlamacppCliStockToolsGgufText);
cell_flags!(EndToEndLatencyLlamacppCliStockToolsGgufText: EndToEndLatencyLlamacppCliStockToolsGgufText);
cell_flags!(EvalLlamacppCliStockToolsGgufText: EvalLlamacppCliStockToolsGgufText);
cell_flags!(VlThroughputLlamacppCliStockToolsGgufVision: VlThroughputLlamacppCliStockToolsGgufVision);
cell_flags!(VlmEvalLlamacppCliStockToolsGgufVision: VlmEvalLlamacppCliStockToolsGgufVision);
cell_flags!(AsrEvalLlamacppCliStockToolsGgufVision: AsrEvalLlamacppCliStockToolsGgufVision);
cell_flags!(VlMaxMemoryLlamacppCliStockToolsGgufVision: VlMaxMemoryLlamacppCliStockToolsGgufVision);
cell_flags!(AsrThroughputLlamacppCliStockToolsGgufVision: AsrThroughputLlamacppCliStockToolsGgufVision);
cell_flags!(AsrMaxMemoryLlamacppCliStockToolsGgufVision: AsrMaxMemoryLlamacppCliStockToolsGgufVision);
cell_flags!(
    VlSpecDecodeLlamacppCliStockTools:
        VlSpecDecodeLlamacppCliStockToolsGgufVision,
        VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative
);
cell_flags!(PrefillThroughputUvOpenvinoOpenvino: PrefillThroughputUvOpenvinoOpenvino);
cell_flags!(DecodeThroughputUvOpenvinoOpenvino: DecodeThroughputUvOpenvinoOpenvino);
cell_flags!(EndToEndLatencyUvOpenvinoOpenvino: EndToEndLatencyUvOpenvinoOpenvino);
cell_flags!(MaxMemoryUsageUvOpenvinoOpenvino: MaxMemoryUsageUvOpenvinoOpenvino);
cell_flags!(PrefillThroughputOvmsOpenvino: PrefillThroughputOvmsOpenvino);
cell_flags!(DecodeThroughputOvmsOpenvino: DecodeThroughputOvmsOpenvino);
cell_flags!(EndToEndLatencyOvmsOpenvino: EndToEndLatencyOvmsOpenvino);
cell_flags!(MaxMemoryUsageOvmsOpenvino: MaxMemoryUsageOvmsOpenvino);
cell_flags!(
    TrtEdgellmGgufVision:
        VlThroughputTrtEdgellmGgufVision,
        VlMaxMemoryTrtEdgellmGgufVision
);

/// Runtime flags for one cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, strum::EnumCount)]
#[cfg_attr(test, derive(strum::IntoStaticStr))]
// The schema describes the variants, not the flat ref serde goes through, and
// `triple_arms` swaps each one's name tag for the axis keys the wire tags it by.
#[cfg_attr(
    feature = "schemars",
    derive(schemars::JsonSchema),
    schemars(!try_from, !into, tag = "variant", transform = crate::json_schema::triple_arms)
)]
#[serde(try_from = "RuntimeFlagRef", into = "RuntimeFlagRef")]
pub enum RuntimeFlags {
    // llama.cpp CLI (stock tools) × gguf-text — bench cells (llama-bench, and
    // llama-batched-bench for a batched prefill or decode).
    //
    // `mmap` stays for plans that author it, though upstream deprecated it for `--load-mode`.
    // `load_mode` wins where both are set, and argv carries one, never both: llama-server
    // keeps the last flag and llama-bench runs a row per mode.
    PrefillThroughputLlamacppCliStockToolsGgufText(PrefillThroughputLlamacppCliStockToolsGgufText),
    DecodeThroughputLlamacppCliStockToolsGgufText(DecodeThroughputLlamacppCliStockToolsGgufText),
    MaxMemoryUsageLlamacppCliStockToolsGgufText(MaxMemoryUsageLlamacppCliStockToolsGgufText),
    // llama.cpp CLI × gguf-text — server cells (llama-server): + ctx_size/no_cache.
    EndToEndLatencyLlamacppCliStockToolsGgufText(EndToEndLatencyLlamacppCliStockToolsGgufText),
    EvalLlamacppCliStockToolsGgufText(EvalLlamacppCliStockToolsGgufText),
    // llama.cpp CLI × gguf-vision — server cells.
    VlThroughputLlamacppCliStockToolsGgufVision(VlThroughputLlamacppCliStockToolsGgufVision),
    // gguf-vision also covers a projector with an audio encoder.
    AsrThroughputLlamacppCliStockToolsGgufVision(AsrThroughputLlamacppCliStockToolsGgufVision),
    VlmEvalLlamacppCliStockToolsGgufVision(VlmEvalLlamacppCliStockToolsGgufVision),
    AsrEvalLlamacppCliStockToolsGgufVision(AsrEvalLlamacppCliStockToolsGgufVision),
    VlMaxMemoryLlamacppCliStockToolsGgufVision(VlMaxMemoryLlamacppCliStockToolsGgufVision),
    AsrMaxMemoryLlamacppCliStockToolsGgufVision(AsrMaxMemoryLlamacppCliStockToolsGgufVision),
    // llama.cpp CLI x gguf-vision corpus cell on a regular VL model: `speculative_enabled`
    // may only be `false` (no drafter), and `raw` may not carry a speculation flag.
    VlSpecDecodeLlamacppCliStockToolsGgufVision(VlSpecDecodeLlamacppCliStockTools),
    // llama.cpp CLI x gguf-vision + drafter, on unless `speculative_enabled = false`. The
    // draft length and drafter are derived, so reserved in `raw`; with speculation off,
    // `raw` may carry no speculation flag.
    VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(VlSpecDecodeLlamacppCliStockTools),
    // Docker vLLM. The throughput cells carry the latency cell's knob set: all three drive
    // one `/v1/completions` request per rep against the same server.
    PrefillThroughputDockerVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    DecodeThroughputDockerVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EndToEndLatencyDockerVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EvalDockerVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        /// Client-side request fan-out: N samples in flight against the one
        /// server. Unlike llama.cpp's `parallel` it reaches no argv, as vLLM and
        /// SGLang batch continuously. Unset or `1` keeps the sequential loop.
        parallel: Option<PositiveCount>,
        /// vLLM's repetition stop, sent on every free-text eval request; reaches no argv.
        /// Its stop is recorded as `doom_loop`. Unset sends no field.
        repetition_detection: Option<VllmRepetitionDetection>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    MaxMemoryUsageDockerVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    PrefillThroughputUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    DecodeThroughputUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EndToEndLatencyUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EvalUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        /// See [`RuntimeFlags::EvalDockerVllmTorch::parallel`].
        parallel: Option<PositiveCount>,
        /// See [`RuntimeFlags::EvalDockerVllmTorch::repetition_detection`].
        repetition_detection: Option<VllmRepetitionDetection>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    // uv vLLM × torch — the VLM accuracy eval. The text eval cell's knobs but
    // `repetition_detection`: its stop would change how a recipe's answers end.
    VlmEvalUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        /// See [`RuntimeFlags::EvalDockerVllmTorch::parallel`].
        parallel: Option<PositiveCount>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    // uv vLLM × torch — the speech-recognition eval, with the vLLM vlm_eval
    // cell's knobs and rules: its entry owns everything that can change an
    // answer.
    AsrEvalUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        /// See [`RuntimeFlags::EvalDockerVllmTorch::parallel`].
        parallel: Option<PositiveCount>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    MaxMemoryUsageUvVllmTorch {
        tensor_parallel_size: Option<PositiveCount>,
        dtype: Option<VllmDtype>,
        max_model_len: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    // Docker SGLang. Context length is left to the operator (`--context-length`), so
    // there's no `max_model_len` here.
    PrefillThroughputDockerSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    DecodeThroughputDockerSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EndToEndLatencyDockerSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EvalDockerSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        /// See [`RuntimeFlags::EvalDockerVllmTorch::parallel`].
        parallel: Option<PositiveCount>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    MaxMemoryUsageDockerSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    PrefillThroughputUvSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    DecodeThroughputUvSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EndToEndLatencyUvSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    EvalUvSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        /// See [`RuntimeFlags::EvalDockerVllmTorch::parallel`].
        parallel: Option<PositiveCount>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    MaxMemoryUsageUvSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
    },
    // SGLang × the corpus benchmark. No `parallel`: the batch is the benchmark's
    // `parameter_batch_size`. With speculation on, the algorithm, drafter and block size
    // are derived, so reserved in `raw`; off, `raw` may carry no `--speculative-*` flag.
    // No vLLM cells: its OpenAI endpoint reports no per-request draft counters.
    VlSpecDecodeDockerSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
        speculative_enabled: Option<bool>,
    },
    VlSpecDecodeDockerSglangTorchSpeculative {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        gpus: Option<String>,
        shm_size: Option<DockerShmSize>,
        ipc: Option<DockerIpcMode>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
        speculative_enabled: Option<bool>,
    },
    VlSpecDecodeUvSglangTorch {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
        speculative_enabled: Option<bool>,
    },
    VlSpecDecodeUvSglangTorchSpeculative {
        tensor_parallel_size: Option<PositiveCount>,
        prefix_caching: Option<bool>,
        envs: Vec<EnvForward>,
        raw: Vec<String>,
        speculative_enabled: Option<bool>,
    },
    // iOS llama.cpp (in-process Metal) × gguf-text: the app load path's knobs, and no `raw`
    // as there is no argv. `threads` matters even at full GPU offload: sampling and
    // CPU-kept ops run there. `swa_full` is stated, not inherited: the CLI tools pin it off,
    // the library defaults it on, and on a SWA model it changes the KV allocation.
    // `load_mode` defaults to `none` (anonymous RAM) in both apps, so memory rows compare
    // with desktop runs. No `mmap` boolean: upstream deprecated it, and these cells never
    // took it, so there is nothing to keep compatible.
    PrefillThroughputLlamacppIosPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    DecodeThroughputLlamacppIosPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    MaxMemoryUsageLlamacppIosPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    EndToEndLatencyLlamacppIosPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    EvalLlamacppIosPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    VlThroughputLlamacppIosPipetteGgufVision {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    VlMaxMemoryLlamacppIosPipetteGgufVision {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    AsrThroughputLlamacppIosPipetteGgufVision {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    AsrMaxMemoryLlamacppIosPipetteGgufVision {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
    },
    /// iOS llama.cpp × gguf-vision corpus cell on a regular model;
    /// `speculative_enabled` may only be `false` here.
    VlSpecDecodeLlamacppIosPipetteGgufVision {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
        speculative_enabled: Option<bool>,
    },
    /// iOS llama.cpp × gguf-vision + drafter; `speculative_enabled` unset means
    /// on. No `envs` or `raw`: an in-process engine has no argv to append to.
    /// The drafter inherits the target's `number_gpu_layers`, `threads` and
    /// `load_mode`: offloading one and not the other would measure the transfer.
    VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        threads: Option<u32>,
        swa_full: Option<bool>,
        load_mode: Option<LlamacppLoadMode>,
        speculative_enabled: Option<bool>,
    },
    // MLX (macOS) × the corpus benchmark: `mlx_vlm.server` runs with fixed arguments, so
    // whether the drafter bundle loads is the only knob.
    VlSpecDecodeMlxMacosPipetteMlx {
        speculative_enabled: Option<bool>,
    },
    VlSpecDecodeMlxMacosPipetteMlxSpeculative {
        speculative_enabled: Option<bool>,
    },
    // Android apk llama.cpp (in-process) × gguf-text: the iOS load path through the
    // `ee_llama_model_load` shim, minus `threads` and `swa_full`, which the shim does not
    // take (it derives `n_threads` itself), so they are refused at parse. A family of its own
    // because the two apps ship independently: a knob may reach one platform first.
    PrefillThroughputLlamacppApkPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        load_mode: Option<LlamacppLoadMode>,
    },
    DecodeThroughputLlamacppApkPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        load_mode: Option<LlamacppLoadMode>,
    },
    MaxMemoryUsageLlamacppApkPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        load_mode: Option<LlamacppLoadMode>,
    },
    EndToEndLatencyLlamacppApkPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        load_mode: Option<LlamacppLoadMode>,
    },
    EvalLlamacppApkPipetteGgufText {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        load_mode: Option<LlamacppLoadMode>,
    },
    VlThroughputLlamacppApkPipetteGgufVision {
        number_gpu_layers: Option<u32>,
        ctx_size: Option<u32>,
        n_ubatch: Option<u32>,
        load_mode: Option<LlamacppLoadMode>,
    },
    // iOS MLX × mlx — only prefill-chunk / ubatch; no raw argv surface.
    PrefillThroughputMlxIosPipetteMlx {
        n_ubatch: Option<u32>,
    },
    DecodeThroughputMlxIosPipetteMlx {
        n_ubatch: Option<u32>,
    },
    MaxMemoryUsageMlxIosPipetteMlx {
        n_ubatch: Option<u32>,
    },
    EndToEndLatencyMlxIosPipetteMlx {
        n_ubatch: Option<u32>,
    },
    EvalMlxIosPipetteMlx {
        n_ubatch: Option<u32>,
    },
    // OpenVINO x IR. No `raw`: the runtime takes typed `LLMPipeline` properties, not a
    // command line. `device` is `Option` like every flag field, but a cell naming none is
    // refused rather than defaulted, since OpenVINO would silently pick CPU.
    PrefillThroughputUvOpenvinoOpenvino(PrefillThroughputUvOpenvinoOpenvino),
    DecodeThroughputUvOpenvinoOpenvino(DecodeThroughputUvOpenvinoOpenvino),
    EndToEndLatencyUvOpenvinoOpenvino(EndToEndLatencyUvOpenvinoOpenvino),
    MaxMemoryUsageUvOpenvinoOpenvino(MaxMemoryUsageUvOpenvinoOpenvino),
    // OVMS x IR: `uv_openvino`'s knobs plus `prefix_caching`. `device` is required here
    // too, since OVMS silently picks a GPU, and `prefix_caching` resolves to off because
    // OVMS defaults it on.
    PrefillThroughputOvmsOpenvino(PrefillThroughputOvmsOpenvino),
    DecodeThroughputOvmsOpenvino(DecodeThroughputOvmsOpenvino),
    EndToEndLatencyOvmsOpenvino(EndToEndLatencyOvmsOpenvino),
    MaxMemoryUsageOvmsOpenvino(MaxMemoryUsageOvmsOpenvino),
    // TensorRT Edge-LLM x gguf-vision: the engine precision, env forwards, and `raw` for the
    // server's own flags. llama-server's tuning knobs mean nothing to it, so none is typed.
    VlThroughputTrtEdgellmGgufVision(TrtEdgellmGgufVision),
    VlMaxMemoryTrtEdgellmGgufVision(TrtEdgellmGgufVision),
}

impl RuntimeFlags {
    /// The `(benchmark, runtime, model)` triple this variant encodes: its cell identity.
    pub fn axes(&self) -> (BenchmarkType, RuntimeType, ModelType) {
        use BenchmarkType as B;
        use ModelType as M;
        use RuntimeType as R;
        match self {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. } => {
                (B::PrefillThroughput, R::LlamacppCliStockTools, M::GgufText)
            }
            RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. } => {
                (B::DecodeThroughput, R::LlamacppCliStockTools, M::GgufText)
            }
            RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. } => {
                (B::MaxMemoryUsage, R::LlamacppCliStockTools, M::GgufText)
            }
            RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. } => {
                (B::EndToEndLatency, R::LlamacppCliStockTools, M::GgufText)
            }
            RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. } => {
                (B::Eval, R::LlamacppCliStockTools, M::GgufText)
            }
            RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlThroughput, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. } => {
                (B::AsrThroughput, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlmEval, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. } => {
                (B::AsrEval, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlMaxMemory, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. } => {
                (B::AsrMaxMemory, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVision)
            }
            RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. } => (
                B::VlSpecDecode,
                R::LlamacppCliStockTools,
                M::GgufVisionSpeculative,
            ),
            RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. } => {
                (B::PrefillThroughput, R::UvOpenvino, M::Openvino)
            }
            RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. } => {
                (B::DecodeThroughput, R::UvOpenvino, M::Openvino)
            }
            RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. } => {
                (B::EndToEndLatency, R::UvOpenvino, M::Openvino)
            }
            RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. } => {
                (B::MaxMemoryUsage, R::UvOpenvino, M::Openvino)
            }
            RuntimeFlags::PrefillThroughputOvmsOpenvino { .. } => {
                (B::PrefillThroughput, R::Ovms, M::Openvino)
            }
            RuntimeFlags::DecodeThroughputOvmsOpenvino { .. } => {
                (B::DecodeThroughput, R::Ovms, M::Openvino)
            }
            RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. } => {
                (B::EndToEndLatency, R::Ovms, M::Openvino)
            }
            RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. } => {
                (B::MaxMemoryUsage, R::Ovms, M::Openvino)
            }
            RuntimeFlags::PrefillThroughputDockerVllmTorch { .. } => {
                (B::PrefillThroughput, R::DockerVllm, M::Torch)
            }
            RuntimeFlags::DecodeThroughputDockerVllmTorch { .. } => {
                (B::DecodeThroughput, R::DockerVllm, M::Torch)
            }
            RuntimeFlags::EndToEndLatencyDockerVllmTorch { .. } => {
                (B::EndToEndLatency, R::DockerVllm, M::Torch)
            }
            RuntimeFlags::EvalDockerVllmTorch { .. } => (B::Eval, R::DockerVllm, M::Torch),
            RuntimeFlags::MaxMemoryUsageDockerVllmTorch { .. } => {
                (B::MaxMemoryUsage, R::DockerVllm, M::Torch)
            }
            RuntimeFlags::PrefillThroughputUvVllmTorch { .. } => {
                (B::PrefillThroughput, R::UvVllm, M::Torch)
            }
            RuntimeFlags::DecodeThroughputUvVllmTorch { .. } => {
                (B::DecodeThroughput, R::UvVllm, M::Torch)
            }
            RuntimeFlags::EndToEndLatencyUvVllmTorch { .. } => {
                (B::EndToEndLatency, R::UvVllm, M::Torch)
            }
            RuntimeFlags::EvalUvVllmTorch { .. } => (B::Eval, R::UvVllm, M::Torch),
            RuntimeFlags::VlmEvalUvVllmTorch { .. } => (B::VlmEval, R::UvVllm, M::Torch),
            RuntimeFlags::AsrEvalUvVllmTorch { .. } => (B::AsrEval, R::UvVllm, M::Torch),
            RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. } => {
                (B::MaxMemoryUsage, R::UvVllm, M::Torch)
            }
            RuntimeFlags::PrefillThroughputDockerSglangTorch { .. } => {
                (B::PrefillThroughput, R::DockerSglang, M::Torch)
            }
            RuntimeFlags::DecodeThroughputDockerSglangTorch { .. } => {
                (B::DecodeThroughput, R::DockerSglang, M::Torch)
            }
            RuntimeFlags::EndToEndLatencyDockerSglangTorch { .. } => {
                (B::EndToEndLatency, R::DockerSglang, M::Torch)
            }
            RuntimeFlags::EvalDockerSglangTorch { .. } => (B::Eval, R::DockerSglang, M::Torch),
            RuntimeFlags::MaxMemoryUsageDockerSglangTorch { .. } => {
                (B::MaxMemoryUsage, R::DockerSglang, M::Torch)
            }
            RuntimeFlags::PrefillThroughputUvSglangTorch { .. } => {
                (B::PrefillThroughput, R::UvSglang, M::Torch)
            }
            RuntimeFlags::DecodeThroughputUvSglangTorch { .. } => {
                (B::DecodeThroughput, R::UvSglang, M::Torch)
            }
            RuntimeFlags::EndToEndLatencyUvSglangTorch { .. } => {
                (B::EndToEndLatency, R::UvSglang, M::Torch)
            }
            RuntimeFlags::EvalUvSglangTorch { .. } => (B::Eval, R::UvSglang, M::Torch),
            RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. } => {
                (B::MaxMemoryUsage, R::UvSglang, M::Torch)
            }
            RuntimeFlags::VlSpecDecodeDockerSglangTorch { .. } => {
                (B::VlSpecDecode, R::DockerSglang, M::Torch)
            }
            RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { .. } => {
                (B::VlSpecDecode, R::DockerSglang, M::TorchSpeculative)
            }
            RuntimeFlags::VlSpecDecodeUvSglangTorch { .. } => {
                (B::VlSpecDecode, R::UvSglang, M::Torch)
            }
            RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. } => {
                (B::VlSpecDecode, R::UvSglang, M::TorchSpeculative)
            }
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. } => {
                (B::PrefillThroughput, R::LlamacppIosPipette, M::GgufText)
            }
            RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. } => {
                (B::DecodeThroughput, R::LlamacppIosPipette, M::GgufText)
            }
            RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. } => {
                (B::MaxMemoryUsage, R::LlamacppIosPipette, M::GgufText)
            }
            RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. } => {
                (B::EndToEndLatency, R::LlamacppIosPipette, M::GgufText)
            }
            RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. } => {
                (B::Eval, R::LlamacppIosPipette, M::GgufText)
            }
            RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. } => {
                (B::VlThroughput, R::LlamacppIosPipette, M::GgufVision)
            }
            RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. } => {
                (B::VlMaxMemory, R::LlamacppIosPipette, M::GgufVision)
            }
            RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. } => {
                (B::AsrThroughput, R::LlamacppIosPipette, M::GgufVision)
            }
            RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. } => {
                (B::AsrMaxMemory, R::LlamacppIosPipette, M::GgufVision)
            }
            RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. } => {
                (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVision)
            }
            RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. } => (
                B::VlSpecDecode,
                R::LlamacppIosPipette,
                M::GgufVisionSpeculative,
            ),
            RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. } => {
                (B::PrefillThroughput, R::LlamacppApkPipette, M::GgufText)
            }
            RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. } => {
                (B::DecodeThroughput, R::LlamacppApkPipette, M::GgufText)
            }
            RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. } => {
                (B::MaxMemoryUsage, R::LlamacppApkPipette, M::GgufText)
            }
            RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. } => {
                (B::EndToEndLatency, R::LlamacppApkPipette, M::GgufText)
            }
            RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. } => {
                (B::Eval, R::LlamacppApkPipette, M::GgufText)
            }
            RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. } => {
                (B::VlThroughput, R::LlamacppApkPipette, M::GgufVision)
            }
            RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. } => {
                (B::VlSpecDecode, R::MlxMacosPipette, M::Mlx)
            }
            RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. } => {
                (B::VlSpecDecode, R::MlxMacosPipette, M::MlxSpeculative)
            }
            RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. } => {
                (B::PrefillThroughput, R::MlxIosPipette, M::Mlx)
            }
            RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. } => {
                (B::DecodeThroughput, R::MlxIosPipette, M::Mlx)
            }
            RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. } => {
                (B::MaxMemoryUsage, R::MlxIosPipette, M::Mlx)
            }
            RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. } => {
                (B::EndToEndLatency, R::MlxIosPipette, M::Mlx)
            }
            RuntimeFlags::EvalMlxIosPipetteMlx { .. } => (B::Eval, R::MlxIosPipette, M::Mlx),
            RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. } => {
                (B::VlThroughput, R::TrtEdgellm, M::GgufVision)
            }
            RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => {
                (B::VlMaxMemory, R::TrtEdgellm, M::GgufVision)
            }
        }
    }

    /// True iff this entry applies to a cell running `benchmark` on `runtime`
    /// with `model`.
    pub fn matches(&self, benchmark: BenchmarkType, runtime: &Runtime, model: &Model) -> bool {
        self.axes() == (benchmark, RuntimeType::of(runtime), ModelType::of(model))
    }

    /// This variant's own settings, as a result submission carries them: a JSON
    /// object of the fields it sets, without the axis keys, which the payload
    /// already names through its descriptors and benchmark id.
    ///
    /// Env values are redacted ([`Self::for_record`]); `raw` rides verbatim, so a
    /// secret belongs in the bare-`K` `envs` inherit form, never in `raw`. The
    /// engines report the flags a cell ran with, so this records the launch, not
    /// the plan; benchmark-fixed flags stay in the recorded argv.
    pub fn submission_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self.for_record())
            // Unreachable: `RuntimeFlags` serializes infallibly. Fall closed on
            // an empty object rather than dropping the record silently.
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(fields) = value.as_object_mut() {
            AXIS_KEYS.iter().for_each(|key| {
                fields.remove(*key);
            });
        }
        value
    }

    /// These flags with every env forward in the form a record carries
    /// ([`EnvForward::for_record`]): values stripped to their names, except in
    /// the `GGML_*` namespace. A forward may hold a credential and the record
    /// leaves the device; llama.cpp itself reads `HF_TOKEN` and `LLAMA_API_KEY`,
    /// so the split is on the key, never the runtime. `GGML_*` values are the
    /// measurement (`GGML_HEXAGON_OPPOLL=1` changes throughput). Not an allowlist
    /// of known `GGML_*` keys: it would silently redact every new upstream knob.
    pub fn for_record(&self) -> Self {
        let mut out = self.clone();
        out.envs_mut().iter_mut().for_each(|e| *e = e.for_record());
        out
    }

    /// The `docker run --gpus` value this cell sets, if any (docker only).
    pub fn gpus(&self) -> Option<&str> {
        match self {
            RuntimeFlags::PrefillThroughputDockerVllmTorch { gpus, .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { gpus, .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { gpus, .. }
            | RuntimeFlags::EvalDockerVllmTorch { gpus, .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { gpus, .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { gpus, .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { gpus, .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { gpus, .. }
            | RuntimeFlags::EvalDockerSglangTorch { gpus, .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { gpus, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { gpus, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { gpus, .. } => {
                gpus.as_deref()
            }
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. }
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => None,
        }
    }

    /// The `docker run --shm-size` value this cell sets, if any (docker only).
    pub fn shm_size(&self) -> Option<&DockerShmSize> {
        match self {
            RuntimeFlags::PrefillThroughputDockerVllmTorch { shm_size, .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { shm_size, .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { shm_size, .. }
            | RuntimeFlags::EvalDockerVllmTorch { shm_size, .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { shm_size, .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { shm_size, .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { shm_size, .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { shm_size, .. }
            | RuntimeFlags::EvalDockerSglangTorch { shm_size, .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { shm_size, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { shm_size, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { shm_size, .. } => {
                shm_size.as_ref()
            }
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. }
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => None,
        }
    }

    /// The `docker run --ipc` value this cell sets, if any (docker only).
    pub fn ipc(&self) -> Option<DockerIpcMode> {
        match self {
            RuntimeFlags::PrefillThroughputDockerVllmTorch { ipc, .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { ipc, .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { ipc, .. }
            | RuntimeFlags::EvalDockerVllmTorch { ipc, .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { ipc, .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { ipc, .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { ipc, .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { ipc, .. }
            | RuntimeFlags::EvalDockerSglangTorch { ipc, .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { ipc, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { ipc, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { ipc, .. } => *ipc,
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. }
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => None,
        }
    }

    /// Env forwards (`K=V`, or bare `K` to inherit) this cell sets; empty for
    /// the in-process, OpenVINO and macOS MLX cells.
    pub fn envs(&self) -> &[EnvForward] {
        match self {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText(
                PrefillThroughputLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText(
                DecodeThroughputLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText(
                MaxMemoryUsageLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText(
                EndToEndLatencyLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                EvalLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(
                VlThroughputLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision(
                AsrThroughputLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(
                VlmEvalLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision(
                AsrEvalLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision(
                VlMaxMemoryLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision(
                AsrMaxMemoryLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                VlSpecDecodeLlamacppCliStockTools { envs, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                VlSpecDecodeLlamacppCliStockTools { envs, .. },
            )
            | RuntimeFlags::PrefillThroughputDockerVllmTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { envs, .. }
            | RuntimeFlags::EvalDockerVllmTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { envs, .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { envs, .. }
            | RuntimeFlags::EvalUvVllmTorch { envs, .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { envs, .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { envs, .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { envs, .. }
            | RuntimeFlags::EvalDockerSglangTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { envs, .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { envs, .. }
            | RuntimeFlags::EvalUvSglangTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { envs, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { envs, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { envs, .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { envs, .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { envs, .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                envs, ..
            })
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                envs, ..
            }) => envs,
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. } => &[],
        }
    }

    /// [`Self::envs`] for in-place edits — see [`Self::for_record`].
    fn envs_mut(&mut self) -> &mut [EnvForward] {
        match self {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText(
                PrefillThroughputLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText(
                DecodeThroughputLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText(
                MaxMemoryUsageLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText(
                EndToEndLatencyLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                EvalLlamacppCliStockToolsGgufText { envs, .. },
            )
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(
                VlThroughputLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision(
                AsrThroughputLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(
                VlmEvalLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision(
                AsrEvalLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision(
                VlMaxMemoryLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision(
                AsrMaxMemoryLlamacppCliStockToolsGgufVision { envs, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                VlSpecDecodeLlamacppCliStockTools { envs, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                VlSpecDecodeLlamacppCliStockTools { envs, .. },
            )
            | RuntimeFlags::PrefillThroughputDockerVllmTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { envs, .. }
            | RuntimeFlags::EvalDockerVllmTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { envs, .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { envs, .. }
            | RuntimeFlags::EvalUvVllmTorch { envs, .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { envs, .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { envs, .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { envs, .. }
            | RuntimeFlags::EvalDockerSglangTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { envs, .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { envs, .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { envs, .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { envs, .. }
            | RuntimeFlags::EvalUvSglangTorch { envs, .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { envs, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { envs, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { envs, .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { envs, .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { envs, .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                envs, ..
            })
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                envs, ..
            }) => envs,
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. } => &mut [],
        }
    }

    /// The `raw` escape-hatch entries authored on this cell.
    pub(super) fn raw(&self) -> &[String] {
        match self {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText(
                PrefillThroughputLlamacppCliStockToolsGgufText { raw, .. },
            )
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText(
                DecodeThroughputLlamacppCliStockToolsGgufText { raw, .. },
            )
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText(
                MaxMemoryUsageLlamacppCliStockToolsGgufText { raw, .. },
            )
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText(
                EndToEndLatencyLlamacppCliStockToolsGgufText { raw, .. },
            )
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                EvalLlamacppCliStockToolsGgufText { raw, .. },
            )
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(
                VlThroughputLlamacppCliStockToolsGgufVision { raw, .. },
            )
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision(
                AsrThroughputLlamacppCliStockToolsGgufVision { raw, .. },
            )
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(
                VlmEvalLlamacppCliStockToolsGgufVision { raw, .. },
            )
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision(
                AsrEvalLlamacppCliStockToolsGgufVision { raw, .. },
            )
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision(
                VlMaxMemoryLlamacppCliStockToolsGgufVision { raw, .. },
            )
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision(
                AsrMaxMemoryLlamacppCliStockToolsGgufVision { raw, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                VlSpecDecodeLlamacppCliStockTools { raw, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                VlSpecDecodeLlamacppCliStockTools { raw, .. },
            )
            | RuntimeFlags::PrefillThroughputDockerVllmTorch { raw, .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { raw, .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { raw, .. }
            | RuntimeFlags::EvalDockerVllmTorch { raw, .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { raw, .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { raw, .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { raw, .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { raw, .. }
            | RuntimeFlags::EvalUvVllmTorch { raw, .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { raw, .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { raw, .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { raw, .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { raw, .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { raw, .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { raw, .. }
            | RuntimeFlags::EvalDockerSglangTorch { raw, .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { raw, .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { raw, .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { raw, .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { raw, .. }
            | RuntimeFlags::EvalUvSglangTorch { raw, .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { raw, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { raw, .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { raw, .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { raw, .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { raw, .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                raw, ..
            })
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision(TrtEdgellmGgufVision { raw, .. }) => {
                raw
            }
            // OpenVINO takes typed pipeline properties, not a command line, so
            // there is no argv escape hatch either.
            RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. } => &[],
            // The mobile engines are in-process, so there is no argv escape hatch.
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. } => &[],
        }
    }

    /// The corpus benchmark's `speculative_enabled` as authored; what a run did
    /// is [`resolve_speculation`] over this and the model type.
    pub fn speculative_enabled(&self) -> Option<bool> {
        match self {
            RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                VlSpecDecodeLlamacppCliStockTools {
                    speculative_enabled,
                    ..
                },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                VlSpecDecodeLlamacppCliStockTools {
                    speculative_enabled,
                    ..
                },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision {
                speculative_enabled,
                ..
            }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                speculative_enabled,
                ..
            }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx {
                speculative_enabled,
            }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                speculative_enabled,
            }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch {
                speculative_enabled,
                ..
            }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                speculative_enabled,
                ..
            }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch {
                speculative_enabled,
                ..
            }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative {
                speculative_enabled,
                ..
            } => *speculative_enabled,
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::PrefillThroughputDockerVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { .. }
            | RuntimeFlags::EvalDockerVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { .. }
            | RuntimeFlags::EvalDockerSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => None,
        }
    }
}

/// Whether a corpus run's drafter is on: the cell's `speculative_enabled`, or,
/// unset, whether the model has a drafter at all. `true` with no drafter is
/// refused: the row would claim a speculation it never did.
pub fn resolve_speculation(
    model: ModelType,
    speculative_enabled: Option<bool>,
) -> Result<bool, RuntimeFlagsError> {
    let has_drafter = matches!(
        model,
        ModelType::GgufVisionSpeculative | ModelType::MlxSpeculative | ModelType::TorchSpeculative
    );
    match speculative_enabled {
        None => Ok(has_drafter),
        Some(true) if !has_drafter => Err(RuntimeFlagsError::SpeculationWithoutDrafter { model }),
        Some(enabled) => Ok(enabled),
    }
}

/// Refuse what vLLM's `RepetitionDetectionParams` would, so a plan fails when it
/// is parsed rather than at its first sample. The fields' type refuses zero.
fn check_repetition_detection(detection: VllmRepetitionDetection) -> Result<(), RuntimeFlagsError> {
    let VllmRepetitionDetection {
        min_pattern_size: min,
        max_pattern_size: max,
        min_count,
    } = detection;
    if min > max {
        Err(RuntimeFlagsError::RepetitionPatternRange { min, max })
    } else if min_count.into_inner() < 2 {
        Err(RuntimeFlagsError::RepetitionMinCount(min_count))
    } else {
        Ok(())
    }
}

/// The [`RuntimeFlagRef`] fields that identify the cell rather than configure it.
const AXIS_KEYS: [&str; 3] = ["runtime_type", "model_type", "benchmark_type"];

/// Flat wire form of [`RuntimeFlags`]: the three axes plus every knob as an
/// optional field. Serde boundary only: [`TryFrom`] routes it into the one
/// variant the axes name and rejects knobs that variant lacks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(all(test, feature = "schemars"), derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RuntimeFlagRef {
    pub runtime_type: RuntimeType,
    pub model_type: ModelType,
    pub benchmark_type: BenchmarkType,
    /// Whether the model's drafter runs, on the corpus benchmark alone. Unset
    /// follows the model; `true` on a model with no drafter is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speculative_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_gpu_layers: Option<u32>,
    /// Deprecated upstream in favor of [`load_mode`](Self::load_mode), and accepted only
    /// on the CLI cells that already took it. `load_mode` wins where a cell sets both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmap: Option<bool>,
    /// llama.cpp's `llama_load_mode` (`--load-mode`): how weights reach memory. Every
    /// llama.cpp cell takes it, CLI and in-process alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_mode: Option<LlamacppLoadMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash_attention: Option<LlamacppFlashAttention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_size: Option<u32>,
    /// llama.cpp / iOS ubatch (prefill chunk size on MLX iOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ubatch: Option<u32>,
    /// llama.cpp sliding-window cache policy (`swa_full`): `true` allocates KV
    /// for the full context on the windowed layers, `false` for the window
    /// alone. iOS cells only — the stock CLI tools set it themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swa_full: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_cache: Option<bool>,
    /// llama-server slot fan-out; the llama.cpp eval cells only. See the
    /// variant field for how it interacts with `ctx_size`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<PositiveCount>,
    /// vLLM's per-request repetition stop; eval vLLM cells only. See the
    /// variant field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_detection: Option<VllmRepetitionDetection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensor_parallel_size: Option<PositiveCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtype: Option<VllmDtype>,
    /// vLLM context bound (`--max-model-len`), covering prompt + output — the
    /// same semantics as llama.cpp's `ctx_size`. The client derives it from the
    /// benchmark when the cell leaves it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_len: Option<PositiveCount>,
    /// The OpenVINO compute device. Substituted at the call, not the install (one
    /// wheel or archive serves all three), so it configures a cell, not a runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<OpenvinoDevice>,
    /// OpenVINO NPU static-shape prompt bound. Left unset the runtime keeps
    /// GenAI's 1024 default, which the standard 512-token suite fits inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_len: Option<PositiveCount>,
    /// OpenVINO NPU output reservation. Unset, the runtime derives it from the
    /// cell — GenAI's 128 default truncates a 256-token generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_response_len: Option<PositiveCount>,
    /// OpenVINO `GENERATE_HINT` (e.g. `BEST_PERF`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_hint: Option<OpenvinoGenerateHint>,
    /// Server-side prefix caching. The benchmarks measure cold prefill+decode,
    /// so the client sets this `false`; a cell asking for `true` is refused at
    /// execution, where the benchmark's policy lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_caching: Option<bool>,
    /// `docker run` GPU allocation (`--gpus`), e.g. `"all"`. Docker cells only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpus: Option<String>,
    /// `docker run --shm-size`, e.g. `"16g"`. Docker cells only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_size: Option<DockerShmSize>,
    /// `docker run --ipc`, e.g. `"host"`. Docker cells only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc: Option<DockerIpcMode>,
    /// TensorRT Edge-LLM's language-model engine precision. TensorRT Edge-LLM cells only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<TrtEdgellmPrecision>,
    /// Env forwards for the launched process (`K=V`, or bare `K` to inherit from
    /// the launching env).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub envs: Vec<EnvForward>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw: Vec<String>,
}

impl RuntimeFlagRef {
    /// An all-unset ref for one cell. Engines overlay what they ran with and
    /// `try_into` a [`RuntimeFlags`], so reported flags are validated like
    /// authored ones.
    pub fn new(
        benchmark_type: BenchmarkType,
        runtime_type: RuntimeType,
        model_type: ModelType,
    ) -> Self {
        Self {
            runtime_type,
            model_type,
            benchmark_type,
            threads: None,
            number_gpu_layers: None,
            mmap: None,
            load_mode: None,
            flash_attention: None,
            ctx_size: None,
            n_ubatch: None,
            swa_full: None,
            no_cache: None,
            parallel: None,
            repetition_detection: None,
            tensor_parallel_size: None,
            dtype: None,
            max_model_len: None,
            device: None,
            max_prompt_len: None,
            min_response_len: None,
            generate_hint: None,
            prefix_caching: None,
            gpus: None,
            shm_size: None,
            ipc: None,
            precision: None,
            envs: Vec::new(),
            raw: Vec::new(),
            speculative_enabled: None,
        }
    }

    /// Reject a knob that's set but not accepted by this cell.
    fn deny(&self, set: bool, knob: &'static str) -> Result<(), RuntimeFlagsError> {
        if set {
            Err(RuntimeFlagsError::KnobNotAllowed {
                knob,
                runtime: self.runtime_type,
                model: self.model_type,
                benchmark: self.benchmark_type,
            })
        } else {
            Ok(())
        }
    }

    fn deny_llama_knobs_except_ubatch(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.threads.is_some(), "threads")?;
        self.deny(self.number_gpu_layers.is_some(), "number_gpu_layers")?;
        self.deny(self.mmap.is_some(), "mmap")?;
        self.deny(self.load_mode.is_some(), "load_mode")?;
        self.deny(self.flash_attention.is_some(), "flash_attention")
    }

    fn deny_llama_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny_llama_knobs_except_ubatch()?;
        self.deny_n_ubatch()
    }

    /// llama-server knobs, `parallel` included, for the families with no server
    /// surface at all.
    fn deny_server_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny_context_knobs()?;
        self.deny_parallel()
    }

    /// llama-server's context and cache knobs, apart from `parallel` so the
    /// eval vLLM/SGLang arms can refuse these and still take it.
    fn deny_context_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.ctx_size.is_some(), "ctx_size")?;
        self.deny(self.no_cache.is_some(), "no_cache")
    }

    /// Sample fan-out is typed on the eval cells only.
    fn deny_parallel(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.parallel.is_some(), "parallel")
    }

    fn deny_n_ubatch(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.n_ubatch.is_some(), "n_ubatch")
    }

    fn deny_threads(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.threads.is_some(), "threads")
    }

    fn deny_swa_full(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.swa_full.is_some(), "swa_full")
    }

    /// CLI-only llama knobs the in-process engines do not expose; `mmap` too, as
    /// they take `load_mode`. Not `threads`, which the iOS family applies.
    fn deny_cli_only_llama_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.mmap.is_some(), "mmap")?;
        self.deny(self.flash_attention.is_some(), "flash_attention")?;
        self.deny(self.no_cache.is_some(), "no_cache")?;
        self.deny_parallel()
    }

    /// Shared rejects for every in-process mobile cell (no docker/vllm/sglang/`raw`
    /// surface).
    fn deny_non_in_process_families(&self) -> Result<(), RuntimeFlagsError> {
        self.deny_torch_server_knobs()?;
        self.deny_docker_launch()?;
        self.deny_envs()?;
        self.deny(!self.raw.is_empty(), "raw")
    }

    /// Every knob only the torch server cells accept.
    fn deny_torch_server_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny_torch_launch_knobs()?;
        self.deny(self.prefix_caching.is_some(), "prefix_caching")
    }

    /// The torch server knobs minus `prefix_caching`, which OVMS shares.
    fn deny_torch_launch_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.tensor_parallel_size.is_some(), "tensor_parallel_size")?;
        self.deny(self.dtype.is_some(), "dtype")?;
        self.deny(self.max_model_len.is_some(), "max_model_len")
    }

    /// The `docker run` launcher settings — only docker cells accept them (uv runs
    /// on the host; the llama.cpp tools aren't containerized).
    fn deny_docker_launch(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.gpus.is_some(), "gpus")?;
        self.deny(self.shm_size.is_some(), "shm_size")?;
        self.deny(self.ipc.is_some(), "ipc")
    }

    /// The OpenVINO pipeline properties — only `uv_openvino` and `ovms` cells
    /// accept them.
    fn deny_openvino_knobs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(self.device.is_some(), "device")?;
        self.deny(self.max_prompt_len.is_some(), "max_prompt_len")?;
        self.deny(self.min_response_len.is_some(), "min_response_len")?;
        self.deny(self.generate_hint.is_some(), "generate_hint")
    }

    /// Env forwards, refused by the in-process engines and OpenVINO (no
    /// subprocess to hand an environment to). The llama.cpp CLI cells take them
    /// because the `GGML_*` backend knobs are read via `getenv`, with no flag.
    fn deny_envs(&self) -> Result<(), RuntimeFlagsError> {
        self.deny(!self.envs.is_empty(), "envs")
    }
}

impl RuntimeFlags {
    /// The knobs alone, for a client that has already resolved its cell from
    /// `--benchmark`, `--runtime` and `--model`. Not [`Self::submission_value`], which
    /// also redacts env values: the client is about to run these and needs them.
    pub fn knobs_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self)
            // Unreachable: `RuntimeFlags` serializes infallibly. Fall closed on an
            // empty object, which the far side reads as "no knobs".
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(fields) = value.as_object_mut() {
            AXIS_KEYS.iter().for_each(|key| {
                fields.remove(*key);
            });
        }
        value
    }

    /// Parse a client's `--runtime-flags`, a JSON object of knobs, against the cell it
    /// resolved. Axis keys and the old one-element array are refused by name, so a stale
    /// invocation fails instead of half-working. An empty object is `None`.
    pub fn from_cell_json(
        json: &str,
        runtime_type: RuntimeType,
        model_type: ModelType,
        benchmark_type: BenchmarkType,
    ) -> Result<Option<Self>, RuntimeFlagsError> {
        let serde_json::Value::Object(mut fields) = serde_json::from_str(json)? else {
            return Err(RuntimeFlagsError::NotAnObject);
        };
        if fields.is_empty() {
            return Ok(None);
        }
        [
            ("runtime_type", serde_json::to_value(runtime_type)?),
            ("model_type", serde_json::to_value(model_type)?),
            ("benchmark_type", serde_json::to_value(benchmark_type)?),
        ]
        .into_iter()
        .try_for_each(|(key, derived)| {
            if fields.contains_key(key) {
                return Err(RuntimeFlagsError::AxisNotAccepted { key });
            }
            fields.insert(key.to_owned(), derived);
            Ok(())
        })?;
        let reference: RuntimeFlagRef = serde_json::from_value(serde_json::Value::Object(fields))?;
        Ok(Some(Self::try_from(reference)?))
    }
}

impl TryFrom<RuntimeFlagRef> for RuntimeFlags {
    type Error = RuntimeFlagsError;

    fn try_from(r: RuntimeFlagRef) -> Result<Self, RuntimeFlagsError> {
        use BenchmarkType as B;
        use ModelType as M;
        use RuntimeType as R;

        // The OpenVINO knobs and those below are refused centrally, not per arm: missing
        // one of twenty-odd arms would be silent, a knob reported and applied to nothing.
        if !matches!(r.runtime_type, R::UvOpenvino | R::Ovms) {
            r.deny_openvino_knobs()?;
        }
        // `swa_full` is the iOS in-process engine's alone.
        if r.runtime_type != R::LlamacppIosPipette {
            r.deny_swa_full()?;
        }
        // `precision` is TensorRT Edge-LLM's engine build alone.
        if r.runtime_type != R::TrtEdgellm {
            r.deny(r.precision.is_some(), "precision")?;
        }
        // `repetition_detection` is a field of vLLM's eval request alone.
        if let Some(detection) = r.repetition_detection {
            if r.benchmark_type == B::Eval && matches!(r.runtime_type, R::DockerVllm | R::UvVllm) {
                check_repetition_detection(detection)?;
            } else {
                r.deny(true, "repetition_detection")?;
            }
        }
        // `speculative_enabled` is the corpus benchmark's alone, one rule for every engine.
        let speculation = if r.benchmark_type == B::VlSpecDecode {
            resolve_speculation(r.model_type, r.speculative_enabled)?
        } else {
            r.deny(r.speculative_enabled.is_some(), "speculative_enabled")?;
            false
        };

        let flags = match (r.benchmark_type, r.runtime_type, r.model_type) {
            (B::PrefillThroughput, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_server_knobs()?;
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText(
                    PrefillThroughputLlamacppCliStockToolsGgufText {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::DecodeThroughput, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_server_knobs()?;
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText(
                    DecodeThroughputLlamacppCliStockToolsGgufText {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::MaxMemoryUsage, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_server_knobs()?;
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText(
                    MaxMemoryUsageLlamacppCliStockToolsGgufText {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::EndToEndLatency, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText(
                    EndToEndLatencyLlamacppCliStockToolsGgufText {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::Eval, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::EvalLlamacppCliStockToolsGgufText(EvalLlamacppCliStockToolsGgufText {
                    threads: r.threads,
                    number_gpu_layers: r.number_gpu_layers,
                    mmap: r.mmap,
                    load_mode: r.load_mode,
                    flash_attention: r.flash_attention,
                    ctx_size: r.ctx_size,
                    no_cache: r.no_cache,
                    parallel: r.parallel,
                    envs: r.envs,
                    raw: r.raw,
                })
            }
            (B::VlThroughput, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(
                    VlThroughputLlamacppCliStockToolsGgufVision {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::AsrEval, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision(
                    AsrEvalLlamacppCliStockToolsGgufVision {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        parallel: r.parallel,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::AsrThroughput, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision(
                    AsrThroughputLlamacppCliStockToolsGgufVision {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::VlmEval, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(
                    VlmEvalLlamacppCliStockToolsGgufVision {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        parallel: r.parallel,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::VlMaxMemory, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision(
                    VlMaxMemoryLlamacppCliStockToolsGgufVision {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::AsrMaxMemory, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision(
                    AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                    },
                )
            }
            (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                    VlSpecDecodeLlamacppCliStockTools {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                        speculative_enabled: r.speculative_enabled,
                    },
                )
            }
            (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVisionSpeculative) => {
                r.deny_n_ubatch()?;
                r.deny_parallel()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                    VlSpecDecodeLlamacppCliStockTools {
                        threads: r.threads,
                        number_gpu_layers: r.number_gpu_layers,
                        mmap: r.mmap,
                        load_mode: r.load_mode,
                        flash_attention: r.flash_attention,
                        ctx_size: r.ctx_size,
                        no_cache: r.no_cache,
                        envs: r.envs,
                        raw: r.raw,
                        speculative_enabled: r.speculative_enabled,
                    },
                )
            }
            (B::PrefillThroughput, R::DockerVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                RuntimeFlags::PrefillThroughputDockerVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::DecodeThroughput, R::DockerVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                RuntimeFlags::DecodeThroughputDockerVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::EndToEndLatency, R::DockerVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                RuntimeFlags::EndToEndLatencyDockerVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::Eval, R::DockerVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_context_knobs()?;
                RuntimeFlags::EvalDockerVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    parallel: r.parallel,
                    repetition_detection: r.repetition_detection,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::MaxMemoryUsage, R::DockerVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                RuntimeFlags::MaxMemoryUsageDockerVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::PrefillThroughput, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::PrefillThroughputUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::DecodeThroughput, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::DecodeThroughputUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::EndToEndLatency, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::EndToEndLatencyUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::Eval, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_context_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::EvalUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    parallel: r.parallel,
                    repetition_detection: r.repetition_detection,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::VlmEval, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_context_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::VlmEvalUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    parallel: r.parallel,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::AsrEval, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_context_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::AsrEvalUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    parallel: r.parallel,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::MaxMemoryUsage, R::UvVllm, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                RuntimeFlags::MaxMemoryUsageUvVllmTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    dtype: r.dtype,
                    max_model_len: r.max_model_len,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::PrefillThroughput, R::DockerSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::PrefillThroughputDockerSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::DecodeThroughput, R::DockerSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::DecodeThroughputDockerSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::EndToEndLatency, R::DockerSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::EndToEndLatencyDockerSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::Eval, R::DockerSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_context_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::EvalDockerSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    parallel: r.parallel,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::MaxMemoryUsage, R::DockerSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::MaxMemoryUsageDockerSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::PrefillThroughput, R::UvSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::PrefillThroughputUvSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::DecodeThroughput, R::UvSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::DecodeThroughputUvSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::EndToEndLatency, R::UvSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::EndToEndLatencyUvSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::Eval, R::UvSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_context_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::EvalUvSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    parallel: r.parallel,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            (B::MaxMemoryUsage, R::UvSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::MaxMemoryUsageUvSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                }
            }
            // SGLang corpus cells. The batch is the benchmark's, so `parallel` is
            // refused like on every non-eval cell.
            (B::VlSpecDecode, R::DockerSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::VlSpecDecodeDockerSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::VlSpecDecode, R::DockerSglang, M::TorchSpeculative) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    gpus: r.gpus,
                    shm_size: r.shm_size,
                    ipc: r.ipc,
                    envs: r.envs,
                    raw: r.raw,
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::VlSpecDecode, R::UvSglang, M::Torch) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::VlSpecDecodeUvSglangTorch {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::VlSpecDecode, R::UvSglang, M::TorchSpeculative) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny(r.dtype.is_some(), "dtype")?;
                r.deny(r.max_model_len.is_some(), "max_model_len")?;
                RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative {
                    tensor_parallel_size: r.tensor_parallel_size,
                    prefix_caching: r.prefix_caching,
                    envs: r.envs,
                    raw: r.raw,
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::PrefillThroughput, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::DecodeThroughput, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::MaxMemoryUsage, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::EndToEndLatency, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::Eval, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::EvalLlamacppIosPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::VlThroughput, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::VlMaxMemory, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::AsrThroughput, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::AsrMaxMemory, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                }
            }
            (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVisionSpeculative) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    threads: r.threads,
                    swa_full: r.swa_full,
                    load_mode: r.load_mode,
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::PrefillThroughput, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_threads()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    load_mode: r.load_mode,
                }
            }
            (B::DecodeThroughput, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_threads()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    load_mode: r.load_mode,
                }
            }
            (B::MaxMemoryUsage, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_threads()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    load_mode: r.load_mode,
                }
            }
            (B::EndToEndLatency, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_threads()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    load_mode: r.load_mode,
                }
            }
            (B::Eval, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_threads()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::EvalLlamacppApkPipetteGgufText {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    load_mode: r.load_mode,
                }
            }
            (B::VlThroughput, R::LlamacppApkPipette, M::GgufVision) => {
                r.deny_cli_only_llama_knobs()?;
                r.deny_threads()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision {
                    number_gpu_layers: r.number_gpu_layers,
                    ctx_size: r.ctx_size,
                    n_ubatch: r.n_ubatch,
                    load_mode: r.load_mode,
                }
            }
            (B::VlSpecDecode, R::MlxMacosPipette, M::Mlx) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_n_ubatch()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx {
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::VlSpecDecode, R::MlxMacosPipette, M::MlxSpeculative) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_n_ubatch()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                    speculative_enabled: r.speculative_enabled,
                }
            }
            (B::PrefillThroughput, R::MlxIosPipette, M::Mlx) => {
                r.deny_llama_knobs_except_ubatch()?;
                r.deny_server_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::PrefillThroughputMlxIosPipetteMlx {
                    n_ubatch: r.n_ubatch,
                }
            }
            (B::DecodeThroughput, R::MlxIosPipette, M::Mlx) => {
                r.deny_llama_knobs_except_ubatch()?;
                r.deny_server_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::DecodeThroughputMlxIosPipetteMlx {
                    n_ubatch: r.n_ubatch,
                }
            }
            (B::MaxMemoryUsage, R::MlxIosPipette, M::Mlx) => {
                r.deny_llama_knobs_except_ubatch()?;
                r.deny_server_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx {
                    n_ubatch: r.n_ubatch,
                }
            }
            (B::EndToEndLatency, R::MlxIosPipette, M::Mlx) => {
                r.deny_llama_knobs_except_ubatch()?;
                r.deny_server_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx {
                    n_ubatch: r.n_ubatch,
                }
            }
            (B::Eval, R::MlxIosPipette, M::Mlx) => {
                r.deny_llama_knobs_except_ubatch()?;
                r.deny_server_knobs()?;
                r.deny_non_in_process_families()?;
                RuntimeFlags::EvalMlxIosPipetteMlx {
                    n_ubatch: r.n_ubatch,
                }
            }
            // OpenVINO x IR. No `raw`, no env forwards, no launcher settings:
            // the driver is spawned directly with typed pipeline properties.
            (B::PrefillThroughput, R::UvOpenvino, M::Openvino)
            | (B::DecodeThroughput, R::UvOpenvino, M::Openvino)
            | (B::EndToEndLatency, R::UvOpenvino, M::Openvino)
            | (B::MaxMemoryUsage, R::UvOpenvino, M::Openvino) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_n_ubatch()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                r.deny_envs()?;
                r.deny(!r.raw.is_empty(), "raw")?;
                let device = r.device;
                let max_prompt_len = r.max_prompt_len;
                let min_response_len = r.min_response_len;
                let generate_hint = r.generate_hint;
                match r.benchmark_type {
                    B::PrefillThroughput => RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino(
                        PrefillThroughputUvOpenvinoOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                        },
                    ),
                    B::DecodeThroughput => RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino(
                        DecodeThroughputUvOpenvinoOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                        },
                    ),
                    B::EndToEndLatency => RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino(
                        EndToEndLatencyUvOpenvinoOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                        },
                    ),
                    B::MaxMemoryUsage => RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino(
                        MaxMemoryUsageUvOpenvinoOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                        },
                    ),
                    // Spelled out rather than `_`, so a new benchmark fails to
                    // compile instead of routing silently.
                    B::Eval
                    | B::VlmEval
                    | B::AsrEval
                    | B::VlThroughput
                    | B::VlMaxMemory
                    | B::AsrMaxMemory
                    | B::VlSpecDecode
                    | B::AsrThroughput => {
                        return Err(RuntimeFlagsError::NoSuchCombination {
                            benchmark: r.benchmark_type,
                            runtime: r.runtime_type,
                            model: r.model_type,
                        });
                    }
                }
            }
            // OVMS x IR. No `raw`: it would need an alias denylist so it cannot
            // re-set a typed knob. Server env comes from the client configuration.
            (B::PrefillThroughput, R::Ovms, M::Openvino)
            | (B::DecodeThroughput, R::Ovms, M::Openvino)
            | (B::EndToEndLatency, R::Ovms, M::Openvino)
            | (B::MaxMemoryUsage, R::Ovms, M::Openvino) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_torch_launch_knobs()?;
                r.deny_docker_launch()?;
                r.deny_envs()?;
                r.deny(!r.raw.is_empty(), "raw")?;
                let device = r.device;
                let max_prompt_len = r.max_prompt_len;
                let min_response_len = r.min_response_len;
                let generate_hint = r.generate_hint;
                let prefix_caching = r.prefix_caching;
                match r.benchmark_type {
                    B::PrefillThroughput => {
                        RuntimeFlags::PrefillThroughputOvmsOpenvino(PrefillThroughputOvmsOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                            prefix_caching,
                        })
                    }
                    B::DecodeThroughput => {
                        RuntimeFlags::DecodeThroughputOvmsOpenvino(DecodeThroughputOvmsOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                            prefix_caching,
                        })
                    }
                    B::EndToEndLatency => {
                        RuntimeFlags::EndToEndLatencyOvmsOpenvino(EndToEndLatencyOvmsOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                            prefix_caching,
                        })
                    }
                    B::MaxMemoryUsage => {
                        RuntimeFlags::MaxMemoryUsageOvmsOpenvino(MaxMemoryUsageOvmsOpenvino {
                            device,
                            max_prompt_len,
                            min_response_len,
                            generate_hint,
                            prefix_caching,
                        })
                    }
                    // Spelled out for the reason the OpenVINO arm above gives.
                    B::Eval
                    | B::VlmEval
                    | B::AsrEval
                    | B::VlThroughput
                    | B::VlMaxMemory
                    | B::AsrMaxMemory
                    | B::VlSpecDecode
                    | B::AsrThroughput => {
                        return Err(RuntimeFlagsError::NoSuchCombination {
                            benchmark: r.benchmark_type,
                            runtime: r.runtime_type,
                            model: r.model_type,
                        });
                    }
                }
            }
            (B::VlThroughput | B::VlMaxMemory, R::TrtEdgellm, M::GgufVision) => {
                r.deny_llama_knobs()?;
                r.deny_server_knobs()?;
                r.deny_torch_server_knobs()?;
                r.deny_docker_launch()?;
                let flags = TrtEdgellmGgufVision {
                    precision: r.precision,
                    envs: r.envs,
                    raw: r.raw,
                };
                if r.benchmark_type == B::VlThroughput {
                    RuntimeFlags::VlThroughputTrtEdgellmGgufVision(flags)
                } else {
                    RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision(flags)
                }
            }
            _ => {
                return Err(RuntimeFlagsError::NoSuchCombination {
                    benchmark: r.benchmark_type,
                    runtime: r.runtime_type,
                    model: r.model_type,
                });
            }
        };
        flags.validate_raw(speculation)?;
        flags.validate_envs()?;
        Ok(flags)
    }
}

impl From<RuntimeFlags> for RuntimeFlagRef {
    fn from(f: RuntimeFlags) -> Self {
        let (benchmark_type, runtime_type, model_type) = f.axes();
        let mut out = RuntimeFlagRef::new(benchmark_type, runtime_type, model_type);
        match f {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText(
                PrefillThroughputLlamacppCliStockToolsGgufText {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    envs,

                    raw,
                },
            )
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText(
                DecodeThroughputLlamacppCliStockToolsGgufText {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    envs,

                    raw,
                },
            )
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText(
                MaxMemoryUsageLlamacppCliStockToolsGgufText {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    envs,

                    raw,
                },
            ) => {
                out.threads = threads;
                out.number_gpu_layers = number_gpu_layers;
                out.mmap = mmap;
                out.load_mode = load_mode;
                out.flash_attention = flash_attention;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision(
                AsrEvalLlamacppCliStockToolsGgufVision {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    parallel,
                    envs,
                    raw,
                },
            ) => {
                out.threads = threads;
                out.number_gpu_layers = number_gpu_layers;
                out.mmap = mmap;
                out.load_mode = load_mode;
                out.flash_attention = flash_attention;
                out.ctx_size = ctx_size;
                out.no_cache = no_cache;
                out.parallel = parallel;
                out.envs = envs;
                out.raw = raw;
            }
            // Split from its siblings: only eval binds `parallel`.
            RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                EvalLlamacppCliStockToolsGgufText {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    parallel,
                    envs,

                    raw,
                },
            ) => {
                out.threads = threads;
                out.number_gpu_layers = number_gpu_layers;
                out.mmap = mmap;
                out.load_mode = load_mode;
                out.flash_attention = flash_attention;
                out.ctx_size = ctx_size;
                out.no_cache = no_cache;
                out.parallel = parallel;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(
                VlmEvalLlamacppCliStockToolsGgufVision {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    parallel,
                    envs,
                    raw,
                },
            ) => {
                out.threads = threads;
                out.number_gpu_layers = number_gpu_layers;
                out.mmap = mmap;
                out.load_mode = load_mode;
                out.flash_attention = flash_attention;
                out.ctx_size = ctx_size;
                out.no_cache = no_cache;
                out.parallel = parallel;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText(
                EndToEndLatencyLlamacppCliStockToolsGgufText {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,

                    raw,
                },
            )
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(
                VlThroughputLlamacppCliStockToolsGgufVision {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,

                    raw,
                },
            )
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision(
                AsrThroughputLlamacppCliStockToolsGgufVision {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,

                    raw,
                },
            )
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision(
                AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,

                    raw,
                },
            )
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision(
                VlMaxMemoryLlamacppCliStockToolsGgufVision {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,
                    raw,
                },
            ) => {
                out.threads = threads;
                out.number_gpu_layers = number_gpu_layers;
                out.mmap = mmap;
                out.load_mode = load_mode;
                out.flash_attention = flash_attention;
                out.ctx_size = ctx_size;
                out.no_cache = no_cache;
                out.envs = envs;
                out.raw = raw;
            }
            // Split from the VL cells: only the corpus cells bind `speculative_enabled`.
            RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                VlSpecDecodeLlamacppCliStockTools {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,
                    raw,
                    speculative_enabled,
                },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                VlSpecDecodeLlamacppCliStockTools {
                    threads,
                    number_gpu_layers,
                    mmap,
                    load_mode,
                    flash_attention,
                    ctx_size,
                    no_cache,
                    envs,
                    raw,
                    speculative_enabled,
                },
            ) => {
                out.threads = threads;
                out.number_gpu_layers = number_gpu_layers;
                out.mmap = mmap;
                out.load_mode = load_mode;
                out.flash_attention = flash_attention;
                out.ctx_size = ctx_size;
                out.no_cache = no_cache;
                out.envs = envs;
                out.raw = raw;
                out.speculative_enabled = speculative_enabled;
            }
            // Split from its siblings: only eval binds `parallel`.
            RuntimeFlags::EvalDockerVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                parallel,
                repetition_detection,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.dtype = dtype;
                out.max_model_len = max_model_len;
                out.prefix_caching = prefix_caching;
                out.gpus = gpus;
                out.shm_size = shm_size;
                out.ipc = ipc;
                out.parallel = parallel;
                out.repetition_detection = repetition_detection;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::PrefillThroughputDockerVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.dtype = dtype;
                out.max_model_len = max_model_len;
                out.prefix_caching = prefix_caching;
                out.gpus = gpus;
                out.shm_size = shm_size;
                out.ipc = ipc;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::EvalUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                parallel,
                repetition_detection,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.dtype = dtype;
                out.max_model_len = max_model_len;
                out.prefix_caching = prefix_caching;
                out.parallel = parallel;
                out.repetition_detection = repetition_detection;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::VlmEvalUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                parallel,
                envs,
                raw,
            }
            | RuntimeFlags::AsrEvalUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                parallel,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.dtype = dtype;
                out.max_model_len = max_model_len;
                out.prefix_caching = prefix_caching;
                out.parallel = parallel;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::PrefillThroughputUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                envs,
                raw,
            }
            | RuntimeFlags::DecodeThroughputUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                envs,
                raw,
            }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                envs,
                raw,
            }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch {
                tensor_parallel_size,
                dtype,
                max_model_len,
                prefix_caching,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.dtype = dtype;
                out.max_model_len = max_model_len;
                out.prefix_caching = prefix_caching;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::EvalDockerSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                parallel,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.prefix_caching = prefix_caching;
                out.gpus = gpus;
                out.shm_size = shm_size;
                out.ipc = ipc;
                out.parallel = parallel;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::PrefillThroughputDockerSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.prefix_caching = prefix_caching;
                out.gpus = gpus;
                out.shm_size = shm_size;
                out.ipc = ipc;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::EvalUvSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                parallel,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.prefix_caching = prefix_caching;
                out.parallel = parallel;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::PrefillThroughputUvSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                envs,
                raw,
            }
            | RuntimeFlags::DecodeThroughputUvSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                envs,
                raw,
            }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                envs,
                raw,
            }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                envs,
                raw,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.prefix_caching = prefix_caching;
                out.envs = envs;
                out.raw = raw;
            }
            RuntimeFlags::VlSpecDecodeDockerSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
                speculative_enabled,
            }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                tensor_parallel_size,
                prefix_caching,
                gpus,
                shm_size,
                ipc,
                envs,
                raw,
                speculative_enabled,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.prefix_caching = prefix_caching;
                out.gpus = gpus;
                out.shm_size = shm_size;
                out.ipc = ipc;
                out.envs = envs;
                out.raw = raw;
                out.speculative_enabled = speculative_enabled;
            }
            RuntimeFlags::VlSpecDecodeUvSglangTorch {
                tensor_parallel_size,
                prefix_caching,
                envs,
                raw,
                speculative_enabled,
            }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative {
                tensor_parallel_size,
                prefix_caching,
                envs,
                raw,
                speculative_enabled,
            } => {
                out.tensor_parallel_size = tensor_parallel_size;
                out.prefix_caching = prefix_caching;
                out.envs = envs;
                out.raw = raw;
                out.speculative_enabled = speculative_enabled;
            }
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
            } => {
                out.number_gpu_layers = number_gpu_layers;
                out.ctx_size = ctx_size;
                out.n_ubatch = n_ubatch;
                out.threads = threads;
                out.swa_full = swa_full;
                out.load_mode = load_mode;
            }
            // Split for the same reason as the CLI corpus cells.
            RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
                speculative_enabled,
            }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                threads,
                swa_full,
                load_mode,
                speculative_enabled,
            } => {
                out.number_gpu_layers = number_gpu_layers;
                out.ctx_size = ctx_size;
                out.n_ubatch = n_ubatch;
                out.threads = threads;
                out.swa_full = swa_full;
                out.load_mode = load_mode;
                out.speculative_enabled = speculative_enabled;
            }
            RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx {
                speculative_enabled,
            }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                speculative_enabled,
            } => {
                out.speculative_enabled = speculative_enabled;
            }
            RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                load_mode,
            }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                load_mode,
            }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                load_mode,
            }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                load_mode,
            }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                load_mode,
            }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision {
                number_gpu_layers,
                ctx_size,
                n_ubatch,
                load_mode,
            } => {
                out.number_gpu_layers = number_gpu_layers;
                out.ctx_size = ctx_size;
                out.n_ubatch = n_ubatch;
                out.load_mode = load_mode;
            }
            RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { n_ubatch }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { n_ubatch }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { n_ubatch }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { n_ubatch }
            | RuntimeFlags::EvalMlxIosPipetteMlx { n_ubatch } => {
                out.n_ubatch = n_ubatch;
            }
            RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino(
                PrefillThroughputUvOpenvinoOpenvino {
                    device,
                    max_prompt_len,
                    min_response_len,
                    generate_hint,
                },
            )
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino(
                DecodeThroughputUvOpenvinoOpenvino {
                    device,
                    max_prompt_len,
                    min_response_len,
                    generate_hint,
                },
            )
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino(
                EndToEndLatencyUvOpenvinoOpenvino {
                    device,
                    max_prompt_len,
                    min_response_len,
                    generate_hint,
                },
            )
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino(MaxMemoryUsageUvOpenvinoOpenvino {
                device,
                max_prompt_len,
                min_response_len,
                generate_hint,
            }) => {
                out.device = device;
                out.max_prompt_len = max_prompt_len;
                out.min_response_len = min_response_len;
                out.generate_hint = generate_hint;
            }
            RuntimeFlags::PrefillThroughputOvmsOpenvino(PrefillThroughputOvmsOpenvino {
                device,
                max_prompt_len,
                min_response_len,
                generate_hint,
                prefix_caching,
            })
            | RuntimeFlags::DecodeThroughputOvmsOpenvino(DecodeThroughputOvmsOpenvino {
                device,
                max_prompt_len,
                min_response_len,
                generate_hint,
                prefix_caching,
            })
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino(EndToEndLatencyOvmsOpenvino {
                device,
                max_prompt_len,
                min_response_len,
                generate_hint,
                prefix_caching,
            })
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino(MaxMemoryUsageOvmsOpenvino {
                device,
                max_prompt_len,
                min_response_len,
                generate_hint,
                prefix_caching,
            }) => {
                out.device = device;
                out.max_prompt_len = max_prompt_len;
                out.min_response_len = min_response_len;
                out.generate_hint = generate_hint;
                out.prefix_caching = prefix_caching;
            }
            RuntimeFlags::VlThroughputTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                precision,
                envs,
                raw,
            })
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision(TrtEdgellmGgufVision {
                precision,
                envs,
                raw,
            }) => {
                out.precision = precision;
                out.envs = envs;
                out.raw = raw;
            }
        }
        out
    }
}

/// Plan-wire fixtures the `cell` test modules share.
#[cfg(test)]
pub(super) mod fixtures {
    use crate::{Model, RuntimeFlags};

    pub(in crate::cell) fn gguf_text() -> anyhow::Result<Model> {
        Ok(toml::from_str(
            "type = \"gguf_text\"\nsource = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\npath = \"m.gguf\"",
        )?)
    }

    pub(in crate::cell) fn torch() -> anyhow::Result<Model> {
        Ok(toml::from_str(
            "type = \"torch\"\nsource = \"huggingface\"\norg = \"x\"\nrepo_name = \"y\"",
        )?)
    }

    pub(in crate::cell) fn parse(wire: &str) -> Result<RuntimeFlags, toml::de::Error> {
        toml::from_str(wire)
    }

    pub(in crate::cell) const SPEC_CELL: &str = "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision_speculative\"\nbenchmark_type = \"vl_spec_decode\"\n";
    pub(in crate::cell) const REGULAR_CELL: &str = "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision\"\nbenchmark_type = \"vl_spec_decode\"\n";
    pub(in crate::cell) const MLX_SPEC_CELL: &str = "runtime_type = \"mlx_macos_pipette\"\nmodel_type = \"mlx_speculative\"\nbenchmark_type = \"vl_spec_decode\"\n";
    pub(in crate::cell) const MLX_REGULAR_CELL: &str = "runtime_type = \"mlx_macos_pipette\"\nmodel_type = \"mlx\"\nbenchmark_type = \"vl_spec_decode\"\n";
    pub(in crate::cell) const VL_THROUGHPUT_CELL: &str = "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision\"\nbenchmark_type = \"vl_throughput\"\n";
    pub(in crate::cell) const SGLANG_SPEC_CELL: &str = "runtime_type = \"uv_sglang\"\nmodel_type = \"torch_speculative\"\nbenchmark_type = \"vl_spec_decode\"\n";
    pub(in crate::cell) const SGLANG_REGULAR_CELL: &str = "runtime_type = \"docker_sglang\"\nmodel_type = \"torch\"\nbenchmark_type = \"vl_spec_decode\"\n";

    /// The parse error for `cell` with `knob` appended, or empty if it parsed.
    pub(in crate::cell) fn refusal(cell: &str, knob: &str) -> String {
        parse(&format!("{cell}{knob}"))
            .map(|_| ())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    pub(in crate::cell) fn axes_wire(benchmark: &str, runtime: &str, model: &str) -> String {
        format!(
            "runtime_type = \"{runtime}\"\nmodel_type = \"{model}\"\nbenchmark_type = \"{benchmark}\"\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use strum::EnumCount as _;

    use super::fixtures::{
        MLX_REGULAR_CELL, MLX_SPEC_CELL, REGULAR_CELL, SGLANG_REGULAR_CELL, SGLANG_SPEC_CELL,
        SPEC_CELL, VL_THROUGHPUT_CELL, axes_wire, gguf_text, parse, refusal, torch,
    };
    use super::*;

    fn llamacpp_cli() -> anyhow::Result<Runtime> {
        Ok(toml::from_str(
            "type = \"llamacpp_cli_stock_tools\"\nsource = \"github_release\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b5000\"\nflavor = \"macos-arm64\"",
        )?)
    }

    /// The DSpark release config on a spec-decoding model: kernel switches and
    /// the speculation type are ordinary `envs` and `raw`, redacted as usual.
    #[test]
    fn a_spec_decoding_run_carries_speculation_in_envs_and_raw() -> anyhow::Result<()> {
        let f = parse(&format!(
            "{SPEC_CELL}flash_attention = \"on\"\nnumber_gpu_layers = 99\nctx_size = 8192\nenvs = [\"DSPARK_FUSED=1\", \"GGML_DSPARK_DELTA=16\"]\nraw = [\"--spec-type\", \"draft-dflash\", \"--spec-draft-n-min\", \"0\"]"
        ))?;
        assert_eq!(
            f.envs().iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["DSPARK_FUSED=1", "GGML_DSPARK_DELTA=16"]
        );
        assert_eq!(
            f.raw(),
            ["--spec-type", "draft-dflash", "--spec-draft-n-min", "0"]
        );
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        assert_eq!(
            f.submission_value()["envs"],
            serde_json::json!(["DSPARK_FUSED", "GGML_DSPARK_DELTA=16"])
        );
        Ok(())
    }

    /// Unset follows the model; set, it is the run's condition, and the model
    /// only bounds it: a model with no drafter cannot enable one.
    #[rstest]
    #[case::spec_default(ModelType::GgufVisionSpeculative, None, Some(true))]
    #[case::spec_off(ModelType::GgufVisionSpeculative, Some(false), Some(false))]
    #[case::spec_on(ModelType::MlxSpeculative, Some(true), Some(true))]
    #[case::regular_default(ModelType::GgufVision, None, Some(false))]
    #[case::regular_off(ModelType::Mlx, Some(false), Some(false))]
    #[case::regular_on(ModelType::GgufVision, Some(true), None)]
    fn speculation_resolves_from_the_flag_and_the_model(
        #[case] model: ModelType,
        #[case] flag: Option<bool>,
        #[case] expected: Option<bool>,
    ) {
        assert_eq!(resolve_speculation(model, flag).ok(), expected);
    }

    /// `speculative_enabled = true` on a regular model is refused when the
    /// plan is parsed, on every engine, and the flag is refused outright on
    /// any other benchmark. MLX's `vl_spec_decode` takes nothing else.
    #[rstest]
    #[case::cli_regular_on(REGULAR_CELL, "speculative_enabled = true", "has no drafter")]
    #[case::mlx_regular_on(MLX_REGULAR_CELL, "speculative_enabled = true", "has no drafter")]
    #[case::another_benchmark(
        VL_THROUGHPUT_CELL,
        "speculative_enabled = false",
        "speculative_enabled"
    )]
    #[case::mlx_raw(MLX_SPEC_CELL, "raw = [\"--x\"]", "raw")]
    #[case::sglang_regular_on(SGLANG_REGULAR_CELL, "speculative_enabled = true", "has no drafter")]
    // The batch is the benchmark's `parameter_batch_size`, not a cell knob.
    #[case::sglang_parallel(SGLANG_SPEC_CELL, "parallel = 4", "parallel")]
    #[case::sglang_no_dtype(SGLANG_SPEC_CELL, "dtype = \"bfloat16\"", "dtype")]
    fn the_flag_is_refused_where_it_cannot_apply(
        #[case] cell: &str,
        #[case] knob: &str,
        #[case] expected: &str,
    ) {
        let message = refusal(cell, knob);
        assert!(message.contains(expected), "{knob}: {message:?}");
    }

    #[rstest]
    #[case::cli(SPEC_CELL)]
    #[case::mlx(MLX_SPEC_CELL)]
    #[case::sglang(SGLANG_SPEC_CELL)]
    fn the_flag_is_recorded(
        #[case] cell: &str,
        #[values(true, false)] enabled: bool,
    ) -> anyhow::Result<()> {
        let f = parse(&format!("{cell}speculative_enabled = {enabled}"))?;
        assert_eq!(f.speculative_enabled(), Some(enabled));
        assert_eq!(
            f.submission_value()["speculative_enabled"],
            serde_json::json!(enabled)
        );
        assert_eq!(parse(cell)?.speculative_enabled(), None);
        Ok(())
    }

    #[test]
    fn prefill_cli_maps_to_bench_variant() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nthreads = 4\nnumber_gpu_layers = 99",
        )?;
        assert!(matches!(
            f,
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
        ));
        assert!(f.matches(
            BenchmarkType::PrefillThroughput,
            &llamacpp_cli()?,
            &gguf_text()?
        ));
        assert!(!f.matches(BenchmarkType::Eval, &llamacpp_cli()?, &gguf_text()?));
        assert!(!f.matches(
            BenchmarkType::PrefillThroughput,
            &llamacpp_cli()?,
            &torch()?
        ));
        Ok(())
    }

    #[test]
    fn eval_cli_maps_to_server_variant() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nctx_size = 8192\nno_cache = true",
        )?;
        assert!(matches!(
            f,
            RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
        ));
        Ok(())
    }

    /// Zero is the only rejected slot count; the high cases pin the absence of
    /// a ceiling, which would have to guess the host's VRAM.
    #[rstest]
    #[case::zero_slots(0, false)]
    #[case::one_slot_is_the_serial_run(1, true)]
    #[case::past_the_measured_knee(128, true)]
    #[case::far_past_any_card_we_have(4096, true)]
    fn eval_parallel_rejects_only_zero(
        #[case] parallel: i32,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let wire = format!(
            "{}parallel = {parallel}\n",
            axes_wire("eval", "llamacpp_cli_stock_tools", "gguf_text")
        );
        match parse(&wire) {
            Ok(RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                EvalLlamacppCliStockToolsGgufText { parallel: got, .. },
            )) => {
                anyhow::ensure!(accepted, "parallel = {parallel} should have been rejected");
                anyhow::ensure!(
                    got.map(i32::from) == Some(parallel),
                    "parallel came back as {got:?}"
                );
            }
            Ok(other) => anyhow::bail!("the eval wire routed to {other:?}"),
            Err(e) => {
                anyhow::ensure!(!accepted, "parallel = {parallel} should have parsed: {e}");
                anyhow::ensure!(format!("{e}").contains("greater than 0"), "got: {e}");
            }
        }
        Ok(())
    }

    /// `--reasoning` / `--reasoning-budget` are closed to `raw` on every
    /// llama-server cell: whether a model thinks is its model flags'
    /// `enable_thinking`, which the client applies, and a raw spelling would
    /// contradict it or toggle thinking where no flag asked for it.
    #[rstest]
    #[case::typed_cell("vlm_eval", "gguf_vision", "--reasoning")]
    #[case::typed_cell_budget("vlm_eval", "gguf_vision", "--reasoning-budget")]
    #[case::untyped_server_cell("eval", "gguf_text", "--reasoning")]
    #[case::asr_eval_cell("asr_eval", "gguf_vision", "--reasoning")]
    #[case::asr_eval_cell_budget("asr_eval", "gguf_vision", "--reasoning-budget")]
    #[case::untyped_server_cell_budget("vl_throughput", "gguf_vision", "--reasoning-budget")]
    fn reasoning_flags_are_closed_to_raw_on_server_cells(
        #[case] benchmark: &str,
        #[case] model: &str,
        #[case] flag: &str,
    ) {
        let wire = format!(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"{model}\"\nbenchmark_type = \"{benchmark}\"\nraw = [\"{flag}\", \"off\"]"
        );
        let error = parse(&wire)
            .err()
            .map(|e| format!("{e}"))
            .unwrap_or_else(|| format!("`{flag}` reached raw unchallenged"));
        assert!(
            error.contains("aliases a typed knob"),
            "`{flag}` on {benchmark} should be closed to raw, got: {error}"
        );
    }

    /// Unset `parallel` serializes without the field: cell keys and
    /// eval-checkpoint digests hash this, so a null would orphan stored ones.
    #[test]
    fn eval_parallel_round_trips_and_is_omitted_when_unset() -> anyhow::Result<()> {
        let axes = axes_wire("eval", "llamacpp_cli_stock_tools", "gguf_text");

        let set = parse(&format!("{axes}parallel = 8\n"))?;
        assert!(matches!(
            set,
            RuntimeFlags::EvalLlamacppCliStockToolsGgufText(EvalLlamacppCliStockToolsGgufText {
                parallel: Some(n),
                ..
            }) if i32::from(n) == 8
        ));
        assert_eq!(parse(&toml::to_string(&set)?)?, set);

        let unset = parse(&axes)?;
        assert!(matches!(
            unset,
            RuntimeFlags::EvalLlamacppCliStockToolsGgufText(EvalLlamacppCliStockToolsGgufText {
                parallel: None,
                ..
            })
        ));
        let emitted = toml::to_string(&unset)?;
        anyhow::ensure!(!emitted.contains("parallel"), "emitted: {emitted}");
        Ok(())
    }

    /// liquid_evals' setting, the one the knob exists to send.
    const LIQUID_EVALS_REPETITION_DETECTION: &str =
        "repetition_detection = { min_pattern_size = 2, max_pattern_size = 128, min_count = 10 }\n";

    /// Both vLLM eval cells carry the stop as authored, round-trip it, and
    /// record it on the submission; unset, it serializes without the key.
    #[rstest]
    #[case::docker("docker_vllm", RuntimeType::DockerVllm)]
    #[case::uv("uv_vllm", RuntimeType::UvVllm)]
    fn repetition_detection_round_trips_and_is_omitted_when_unset(
        #[case] runtime: &str,
        #[case] runtime_type: RuntimeType,
    ) -> anyhow::Result<()> {
        let axes = axes_wire("eval", runtime, "torch");
        let expected = VllmRepetitionDetection {
            min_pattern_size: PositiveCount::try_new(2)?,
            max_pattern_size: PositiveCount::try_new(128)?,
            min_count: PositiveCount::try_new(10)?,
        };

        let set = parse(&format!("{axes}{LIQUID_EVALS_REPETITION_DETECTION}"))?;
        assert_eq!(
            set.axes(),
            (BenchmarkType::Eval, runtime_type, ModelType::Torch)
        );
        assert_eq!(
            RuntimeFlagRef::from(set.clone()).repetition_detection,
            Some(expected)
        );
        assert_eq!(parse(&toml::to_string(&set)?)?, set);
        assert_eq!(
            set.submission_value()["repetition_detection"],
            serde_json::json!({"min_pattern_size": 2, "max_pattern_size": 128, "min_count": 10})
        );

        let unset = parse(&axes)?;
        let emitted = serde_json::to_string(&unset)?;
        anyhow::ensure!(
            !emitted.contains("repetition_detection"),
            "emitted: {emitted}"
        );
        assert_eq!(unset.submission_value(), serde_json::json!({}));
        Ok(())
    }

    /// What vLLM's `RepetitionDetectionParams` would refuse is refused when
    /// the plan is parsed, and so is a table that leaves a field to vLLM's
    /// default or carries one vLLM does not have.
    #[rstest]
    #[case::min_above_max(
        "{ min_pattern_size = 129, max_pattern_size = 128, min_count = 10 }",
        "min_pattern_size 129 exceeds max_pattern_size 128"
    )]
    #[case::one_repeat(
        "{ min_pattern_size = 2, max_pattern_size = 128, min_count = 1 }",
        "min_count must be at least 2, got 1"
    )]
    #[case::zero_size_is_vllms_off(
        "{ min_pattern_size = 0, max_pattern_size = 128, min_count = 10 }",
        "greater than 0"
    )]
    #[case::missing_field("{ max_pattern_size = 128, min_count = 10 }", "min_pattern_size")]
    #[case::unknown_field(
        "{ min_pattern_size = 2, max_pattern_size = 128, min_count = 10, window = 4 }",
        "window"
    )]
    #[case::not_a_table("true", "repetition_detection")]
    fn repetition_detection_vllm_would_refuse_is_refused(
        #[case] value: &str,
        #[case] expected: &str,
        #[values("docker_vllm", "uv_vllm")] runtime: &str,
    ) {
        let message = refusal(
            &axes_wire("eval", runtime, "torch"),
            &format!("repetition_detection = {value}\n"),
        );
        assert!(message.contains(expected), "{value}: {message:?}");
    }

    /// The bounds are vLLM's, not tighter: a single-token pattern, two
    /// repeats and equal sizes all parse.
    #[test]
    fn repetition_detection_takes_vllms_smallest_setting() -> anyhow::Result<()> {
        parse(&format!(
            "{}repetition_detection = {{ min_pattern_size = 1, max_pattern_size = 1, min_count = 2 }}\n",
            axes_wire("eval", "uv_vllm", "torch")
        ))?;
        Ok(())
    }

    /// Only vLLM reads the field: SGLang's eval cells share the request
    /// builder but are refused the knob by name, as is every non-eval vLLM
    /// cell.
    #[rstest]
    #[case::sglang_eval("eval", "uv_sglang")]
    #[case::docker_sglang_eval("eval", "docker_sglang")]
    #[case::vllm_latency("end_to_end_latency", "uv_vllm")]
    fn repetition_detection_is_refused_off_the_vllm_eval_cells(
        #[case] benchmark: &str,
        #[case] runtime: &str,
    ) {
        let message = refusal(
            &axes_wire(benchmark, runtime, "torch"),
            LIQUID_EVALS_REPETITION_DETECTION,
        );
        assert!(
            message.contains("knob `repetition_detection` is not accepted"),
            "{message:?}"
        );
    }

    #[rstest]
    // ctx_size is a llama-server knob; prefill is a bench cell.
    #[case::server_knob_on_bench_cell(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nctx_size = 8192"
    )]
    // The in-process engines take `load_mode`, not the boolean upstream deprecated.
    #[case::mmap_boolean_on_an_apk_cell(
        "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nmmap = false"
    )]
    #[case::mmap_boolean_on_an_ios_cell(
        "runtime_type = \"llamacpp_ios_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nmmap = false"
    )]
    // The apk cells refuse the CLI-only knobs and the two the Android engine does not apply.
    #[case::threads_on_an_apk_cell(
        "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nthreads = 6"
    )]
    #[case::swa_full_on_an_apk_cell(
        "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nswa_full = true"
    )]
    #[case::argv_knob_on_an_apk_cell(
        "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nflash_attention = \"on\""
    )]
    // `load_mode` is a llama.cpp knob; MLX has no such concept.
    #[case::load_mode_on_an_mlx_cell(
        "runtime_type = \"mlx_ios_pipette\"\nmodel_type = \"mlx\"\nbenchmark_type = \"eval\"\nload_mode = \"mmap\""
    )]
    // AFM has no typed-flag cells, so any entry is NoSuchCombination.
    #[case::apple_foundation_has_no_variant(
        "runtime_type = \"apple_foundation\"\nmodel_type = \"apple_foundation_text\"\nbenchmark_type = \"eval\""
    )]
    #[case::torch_never_pairs_with_llamacpp(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\""
    )]
    // Whether a model thinks is a model flag (`enable_thinking`), never a
    // runtime flag: a `reasoning` knob is refused on every cell.
    #[case::reasoning_on_a_vlm_eval_cell(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision\"\nbenchmark_type = \"vlm_eval\"\nreasoning = \"on\""
    )]
    // An ASR model never thinks: the asr_eval cell has no reasoning knob, and
    // even `off` is refused, as on the other cells that fix their generation.
    #[case::reasoning_on_an_asr_eval_cell(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision\"\nbenchmark_type = \"asr_eval\"\nreasoning = \"off\""
    )]
    #[case::asr_eval_takes_no_n_ubatch(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision\"\nbenchmark_type = \"asr_eval\"\nn_ubatch = 512"
    )]
    #[case::reasoning_on_a_vl_throughput_cell(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_vision\"\nbenchmark_type = \"vl_throughput\"\nreasoning = \"on\""
    )]
    // dtype is a vLLM knob; sglang doesn't take it.
    #[case::sglang_rejects_dtype(
        "runtime_type = \"docker_sglang\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\ndtype = \"bfloat16\""
    )]
    // `-t` is the typed-`threads` alias — must go through the knob.
    #[case::raw_aliasing_a_typed_knob(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nraw = [\"-t\", \"8\"]"
    )]
    // llama-bench fixes --repetitions (one rep per invocation, looped
    // externally), so a raw override is reserved on every bench cell.
    #[case::raw_repetitions_reserved_on_bench(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nraw = [\"--repetitions\", \"5\"]"
    )]
    // The depth sizes llama-bench's context, so an override would be measured
    // against a deep KV cache and recorded as an ordinary prefill.
    #[case::raw_depth_reserved_on_prefill(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nraw = [\"-d\", \"32768\"]"
    )]
    // Same flag, same context arithmetic, and this cell reports the memory that
    // arithmetic decides.
    #[case::raw_depth_reserved_on_max_memory(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"max_memory_usage\"\nraw = [\"--n-depth\", \"32768\"]"
    )]
    // getopt glued short form `-t8` still aliases the typed `threads` knob.
    #[case::raw_glued_short_form(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nraw = [\"-t8\"]"
    )]
    // gpus/shm_size/ipc are `docker run` options; uv runs on the host.
    #[case::uv_rejects_docker_gpus(
        "runtime_type = \"uv_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\ngpus = \"all\""
    )]
    // The in-process mobile engines have no subprocess to hand an environment to.
    #[case::apk_rejects_envs(
        "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nenvs = [\"GGML_HEXAGON_OPPOLL=1\"]"
    )]
    // A forward whose name no `execve` could carry is refused at plan time, by
    // `EnvForward` at the serde boundary.
    #[case::malformed_env_name(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nenvs = [\"GGML HEXAGON=1\"]"
    )]
    #[case::env_with_no_name(
        "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nenvs = [\"=1\"]"
    )]
    fn rejected_wire_forms(#[case] wire: &str) {
        assert!(parse(wire).is_err());
    }

    #[test]
    fn ios_llama_knobs_round_trip() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"llamacpp_ios_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nnumber_gpu_layers = 99\nctx_size = 8192\nn_ubatch = 512\nthreads = 6\nswa_full = false\nload_mode = \"mmap\"",
        )?;
        assert!(matches!(
            f,
            RuntimeFlags::EvalLlamacppIosPipetteGgufText {
                number_gpu_layers: Some(99),
                ctx_size: Some(8192),
                n_ubatch: Some(512),
                threads: Some(6),
                swa_full: Some(false),
                load_mode: Some(LlamacppLoadMode::Mmap),
            }
        ));
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        Ok(())
    }

    /// The Android family is the iOS family's twin, so it earns the same round-trip: the
    /// two apps run the same engine and a knob that routes on one must route on the other.
    #[test]
    fn apk_llama_knobs_round_trip() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nnumber_gpu_layers = 99\nctx_size = 8192\nn_ubatch = 512\nload_mode = \"mmap\"",
        )?;
        assert!(matches!(
            f,
            RuntimeFlags::EvalLlamacppApkPipetteGgufText {
                number_gpu_layers: Some(99),
                ctx_size: Some(8192),
                n_ubatch: Some(512),
                load_mode: Some(LlamacppLoadMode::Mmap),
            }
        ));
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        Ok(())
    }

    /// Every load mode upstream defines survives the plan wire into the cell.
    #[rstest]
    #[case::none("none", LlamacppLoadMode::Read)]
    #[case::mmap("mmap", LlamacppLoadMode::Mmap)]
    #[case::mlock("mlock", LlamacppLoadMode::Mlock)]
    #[case::mmap_mlock("mmap_mlock", LlamacppLoadMode::MmapMlock)]
    #[case::direct_io("direct_io", LlamacppLoadMode::DirectIo)]
    fn load_mode_spellings(
        #[case] wire: &str,
        #[case] expected: LlamacppLoadMode,
    ) -> anyhow::Result<()> {
        let f = parse(&format!(
            "runtime_type = \"llamacpp_apk_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nload_mode = \"{wire}\""
        ))?;
        let RuntimeFlags::EvalLlamacppApkPipetteGgufText { load_mode, .. } = f else {
            anyhow::bail!("routed to the wrong variant: {f:?}");
        };
        assert_eq!(load_mode, Some(expected));
        // And the wire form round-trips, so a stored plan reparses to the same mode.
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        Ok(())
    }

    /// The in-process engine takes `threads` (llama's `n_threads`), unlike the argv-only
    /// knobs beside it.
    #[test]
    fn ios_llama_takes_threads_but_not_the_argv_knobs() -> anyhow::Result<()> {
        let head = "runtime_type = \"llamacpp_ios_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\n";
        assert!(parse(&format!("{head}threads = 4")).is_ok());
        for argv_only in ["mmap = true", "flash_attention = \"on\"", "no_cache = true"] {
            assert!(
                parse(&format!("{head}{argv_only}")).is_err(),
                "{argv_only} has no in-process counterpart"
            );
        }
        Ok(())
    }

    #[test]
    fn ios_mlx_rejects_ngl() {
        assert!(parse(
            "runtime_type = \"mlx_ios_pipette\"\nmodel_type = \"mlx\"\nbenchmark_type = \"prefill_throughput\"\nnumber_gpu_layers = 1",
        )
        .is_err());
    }

    #[test]
    fn ios_rejects_raw() {
        // In-process iOS engines have no argv surface for a raw escape hatch.
        assert!(parse(
            "runtime_type = \"llamacpp_ios_pipette\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"eval\"\nraw = [\"--foo\"]",
        )
        .is_err());
        assert!(parse(
            "runtime_type = \"mlx_ios_pipette\"\nmodel_type = \"mlx\"\nbenchmark_type = \"prefill_throughput\"\nraw = [\"x\"]",
        )
        .is_err());
    }

    #[test]
    fn vllm_knobs_and_raw_round_trip() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"docker_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\ntensor_parallel_size = 2\nmax_model_len = 4096\nraw = [\"--swap-space\", \"8\"]",
        )?;
        assert!(matches!(f, RuntimeFlags::EvalDockerVllmTorch { .. }));
        let wire = toml::to_string(&f)?;
        assert!(wire.contains("runtime_type = \"docker_vllm\""));
        assert_eq!(parse(&wire)?, f);
        Ok(())
    }

    /// sglang's context length stays an operator concern (`--context-length`),
    /// so the cell has no `max_model_len` to set.
    #[test]
    fn sglang_rejects_max_model_len() {
        let result = parse(
            "runtime_type = \"docker_sglang\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\nmax_model_len = 4096",
        );
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(format!("{e}").contains("max_model_len"), "got: {e}");
        }
    }

    /// The submission shape: the cell's own settings, with env forwards
    /// reduced to names so a token can't ride along.
    #[test]
    fn submission_value_reports_the_cells_settings() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"docker_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\ntensor_parallel_size = 2\nmax_model_len = 8448\nprefix_caching = false\ngpus = \"all\"\nenvs = [\"HF_TOKEN=hunter2\", \"NCCL_DEBUG\"]\nraw = [\"--swap-space\", \"8\"]",
        )?;
        assert_eq!(
            f.submission_value(),
            serde_json::json!({
                "tensor_parallel_size": 2,
                "max_model_len": 8448,
                "prefix_caching": false,
                "gpus": "all",
                "envs": ["HF_TOKEN", "NCCL_DEBUG"],
                "raw": ["--swap-space", "8"],
            })
        );
        Ok(())
    }

    /// The env record splits on the key, not the runtime: a `GGML_*` value
    /// survives, anything else keeps only its name, even on a llama.cpp cell.
    #[test]
    fn ggml_env_values_survive_the_record_but_credentials_do_not() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\n\
             envs = [\"GGML_HEXAGON_OPPOLL=1\", \"GGML_HEXAGON_NHVX=4\", \"HF_TOKEN=hunter2\", \"LLAMA_API_KEY=sekrit\", \"NCCL_DEBUG\"]",
        )?;
        assert_eq!(
            f.submission_value(),
            serde_json::json!({
                "envs": [
                    "GGML_HEXAGON_OPPOLL=1",
                    "GGML_HEXAGON_NHVX=4",
                    "HF_TOKEN",
                    "LLAMA_API_KEY",
                    "NCCL_DEBUG",
                ],
            })
        );
        // The cell itself still carries every value — only the record is redacted,
        // or the run would be configured differently from what it reports.
        assert!(f.envs().iter().any(|e| e.as_ref() == "HF_TOKEN=hunter2"));
        Ok(())
    }

    /// A cell that configured nothing submits an empty object: the axis keys
    /// are the payload's job, not this record's.
    #[test]
    fn submission_value_is_empty_when_the_cell_configures_nothing() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"docker_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"",
        )?;
        assert_eq!(f.submission_value(), serde_json::json!({}));
        Ok(())
    }

    /// A docker cell carries the `docker run` launcher settings; the accessors read
    /// them back and the whole set survives the wire round-trip.
    #[test]
    fn docker_launch_settings_round_trip() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"docker_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"end_to_end_latency\"\ngpus = \"all\"\nshm_size = \"16g\"\nipc = \"host\"\nenvs = [\"HF_TOKEN=x\"]",
        )?;
        assert!(matches!(
            f,
            RuntimeFlags::EndToEndLatencyDockerVllmTorch { .. }
        ));
        assert_eq!(f.gpus(), Some("all"));
        assert_eq!(f.shm_size().map(AsRef::as_ref), Some("16g"));
        assert_eq!(f.ipc(), Some(DockerIpcMode::Host));
        assert_eq!(
            f.envs().iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            ["HF_TOKEN=x"]
        );
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        Ok(())
    }

    /// One accepted and one refused value per knob, proving each value type is
    /// wired into the flags. The grammars themselves are pinned where they live.
    #[rstest]
    #[case::dtype_word("docker_vllm", "torch", "dtype = \"bfloat16\"", true)]
    #[case::dtype_shorthand("docker_vllm", "torch", "dtype = \"bf16\"", false)]
    #[case::ipc_host("docker_vllm", "torch", "ipc = \"host\"", true)]
    #[case::ipc_container("docker_vllm", "torch", "ipc = \"container:other\"", false)]
    #[case::shm_upper_suffixed("docker_vllm", "torch", "shm_size = \"16GB\"", true)]
    #[case::shm_unknown_unit("docker_vllm", "torch", "shm_size = \"16x\"", false)]
    #[case::tp_one("docker_vllm", "torch", "tensor_parallel_size = 1", true)]
    #[case::tp_zero("docker_vllm", "torch", "tensor_parallel_size = 0", false)]
    #[case::max_model_len_zero("docker_vllm", "torch", "max_model_len = 0", false)]
    #[case::max_prompt_len_zero("uv_openvino", "openvino", "max_prompt_len = 0", false)]
    #[case::min_response_len_zero("ovms", "openvino", "min_response_len = 0", false)]
    fn typed_knob_values(
        #[case] runtime: &str,
        #[case] model: &str,
        #[case] knob: &str,
        #[case] accepted: bool,
    ) {
        let wire = format!(
            "{}{knob}\n",
            axes_wire("end_to_end_latency", runtime, model)
        );
        match parse(&wire) {
            Ok(_) => assert!(accepted, "{knob} should have been refused"),
            Err(e) => assert!(!accepted && e.to_string().contains(knob), "{knob}: {e}"),
        }
    }

    /// Digests hash this serialization, so it must match what was authored.
    #[test]
    fn typed_knobs_keep_their_wire_form() -> anyhow::Result<()> {
        let wire = format!(
            "{}tensor_parallel_size = 2\ndtype = \"bfloat16\"\nshm_size = \"16GB\"\nipc = \"host\"\n",
            axes_wire("end_to_end_latency", "docker_vllm", "torch")
        );
        let emitted = toml::to_string(&parse(&wire)?)?;
        [
            "tensor_parallel_size = 2",
            "dtype = \"bfloat16\"",
            "shm_size = \"16GB\"",
            "ipc = \"host\"",
        ]
        .iter()
        .try_for_each(|line| {
            anyhow::ensure!(emitted.contains(line), "{line} missing from:\n{emitted}");
            Ok(())
        })
    }

    /// A uv cell carries `envs` but never the docker-only launcher settings — the
    /// accessors return `None` for them regardless of the variant.
    #[test]
    fn uv_carries_envs_but_not_docker_settings() -> anyhow::Result<()> {
        let f = parse(
            "runtime_type = \"uv_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\nenvs = [\"VLLM_USE_V1=1\"]",
        )?;
        assert!(matches!(f, RuntimeFlags::EvalUvVllmTorch { .. }));
        assert_eq!(
            f.envs().iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            ["VLLM_USE_V1=1"]
        );
        assert_eq!(f.gpus(), None);
        assert_eq!(f.shm_size(), None);
        assert_eq!(f.ipc(), None);
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        Ok(())
    }

    /// Every optional field on [`RuntimeFlagRef`] with a valid wire value. `raw`
    /// uses a flag no denylist names, so a rejection means the cell refuses `raw`.
    const PROBES: &[(&str, &str)] = &[
        ("threads", "4"),
        ("number_gpu_layers", "99"),
        ("mmap", "true"),
        ("load_mode", "\"mmap_mlock\""),
        ("flash_attention", "\"auto\""),
        ("ctx_size", "8192"),
        ("n_ubatch", "512"),
        ("swa_full", "true"),
        ("no_cache", "true"),
        ("parallel", "4"),
        (
            "repetition_detection",
            "{ min_pattern_size = 2, max_pattern_size = 128, min_count = 10 }",
        ),
        ("tensor_parallel_size", "2"),
        ("dtype", "\"bfloat16\""),
        ("max_model_len", "8192"),
        ("device", "\"gpu\""),
        ("max_prompt_len", "4096"),
        ("min_response_len", "256"),
        ("generate_hint", "\"best-perf\""),
        ("prefix_caching", "true"),
        ("gpus", "\"all\""),
        ("shm_size", "\"16g\""),
        ("ipc", "\"host\""),
        ("precision", "\"fp16\""),
        // One every cell forwards, the vLLM vlm_eval cell's allowlist included.
        ("envs", "[\"OMP_NUM_THREADS=1\"]"),
        // A value, not a flag: every cell takes it, the vLLM vlm_eval cell's
        // allowlist included (`raw_allowlist`).
        ("raw", "[\"pipette-field-probe\"]"),
        // `false`: the one value every corpus cell accepts, since `true` needs a
        // drafter and the regular-model cells have none.
        ("speculative_enabled", "false"),
    ];

    /// Every cell that has a variant, with its accepted fields. Transcribed, not
    /// derived, so a variant field changed without an edit here fails the sweep.
    #[rustfmt::skip]
    const CELLS: &[(&str, &str, &str, &[&str])] = &[
        // llama.cpp CLI bench cells (llama-bench): no server or ubatch fields.
        ("prefill_throughput", "llamacpp_cli_stock_tools", "gguf_text",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "envs", "raw"]),
        ("decode_throughput", "llamacpp_cli_stock_tools", "gguf_text",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "envs", "raw"]),
        ("max_memory_usage", "llamacpp_cli_stock_tools", "gguf_text",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "envs", "raw"]),
        // llama.cpp CLI server cells (llama-server).
        ("end_to_end_latency", "llamacpp_cli_stock_tools", "gguf_text",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw"]),
        ("eval", "llamacpp_cli_stock_tools", "gguf_text",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "parallel", "envs", "raw"]),
        ("vl_throughput", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw"]),
        ("asr_throughput", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw"]),
        ("vl_max_memory", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw"]),
        ("asr_max_memory", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw"]),
        // + parallel, as on the text eval.
        ("vlm_eval", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "parallel", "envs", "raw"]),
        ("asr_eval", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "parallel", "envs", "raw"]),
        ("vl_spec_decode", "llamacpp_cli_stock_tools", "gguf_vision",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw",
           "speculative_enabled"]),
        ("vl_spec_decode", "llamacpp_cli_stock_tools", "gguf_vision_speculative",
         &["threads", "number_gpu_layers", "mmap", "load_mode", "flash_attention", "ctx_size", "no_cache", "envs", "raw",
           "speculative_enabled"]),
        // Docker vLLM: server fields + `docker run` launcher settings + envs.
        ("prefill_throughput", "docker_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching",
           "gpus", "shm_size", "ipc", "envs", "raw"]),
        ("decode_throughput", "docker_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching",
           "gpus", "shm_size", "ipc", "envs", "raw"]),
        ("end_to_end_latency", "docker_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching",
           "gpus", "shm_size", "ipc", "envs", "raw"]),
        // Eval also takes vLLM's own request-level repetition stop.
        ("eval", "docker_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching",
           "gpus", "shm_size", "ipc", "parallel", "repetition_detection", "envs", "raw"]),
        ("max_memory_usage", "docker_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching",
           "gpus", "shm_size", "ipc", "envs", "raw"]),
        // Uv vLLM: same server fields, no launcher settings (runs on the host).
        ("prefill_throughput", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "envs", "raw"]),
        ("decode_throughput", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "envs", "raw"]),
        ("end_to_end_latency", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "envs", "raw"]),
        ("eval", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "parallel",
           "repetition_detection", "envs", "raw"]),
        ("vlm_eval", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "parallel",
           "envs", "raw"]),
        ("asr_eval", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "parallel",
           "envs", "raw"]),
        ("max_memory_usage", "uv_vllm", "torch",
         &["tensor_parallel_size", "dtype", "max_model_len", "prefix_caching", "envs", "raw"]),
        // Docker SGLang: as docker vLLM but no `dtype`.
        ("prefill_throughput", "docker_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "envs", "raw"]),
        ("decode_throughput", "docker_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "envs", "raw"]),
        ("end_to_end_latency", "docker_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "envs", "raw"]),
        ("eval", "docker_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "parallel",
           "envs", "raw"]),
        ("max_memory_usage", "docker_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "envs", "raw"]),
        // Uv SGLang: tensor_parallel_size + envs only.
        ("prefill_throughput", "uv_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "envs", "raw"]),
        ("decode_throughput", "uv_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "envs", "raw"]),
        ("end_to_end_latency", "uv_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "envs", "raw"]),
        ("eval", "uv_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "parallel", "envs", "raw"]),
        ("max_memory_usage", "uv_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "envs", "raw"]),
        // The SGLang corpus cells, on a regular model and on the pair.
        ("vl_spec_decode", "docker_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "envs", "raw",
           "speculative_enabled"]),
        ("vl_spec_decode", "docker_sglang", "torch_speculative",
         &["tensor_parallel_size", "prefix_caching", "gpus", "shm_size", "ipc", "envs", "raw",
           "speculative_enabled"]),
        ("vl_spec_decode", "uv_sglang", "torch",
         &["tensor_parallel_size", "prefix_caching", "envs", "raw", "speculative_enabled"]),
        ("vl_spec_decode", "uv_sglang", "torch_speculative",
         &["tensor_parallel_size", "prefix_caching", "envs", "raw", "speculative_enabled"]),
        // The in-process iOS llama.cpp engine: app load-path fields, no CLI argv surface.
        ("prefill_throughput", "llamacpp_ios_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("decode_throughput", "llamacpp_ios_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("max_memory_usage", "llamacpp_ios_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("end_to_end_latency", "llamacpp_ios_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("eval", "llamacpp_ios_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("vl_throughput", "llamacpp_ios_pipette", "gguf_vision",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("vl_max_memory", "llamacpp_ios_pipette", "gguf_vision",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("asr_throughput", "llamacpp_ios_pipette", "gguf_vision",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        ("asr_max_memory", "llamacpp_ios_pipette", "gguf_vision",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode"]),
        // Both iOS corpus cells: the same six plus the drafter switch.
        ("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode",
           "speculative_enabled"]),
        ("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision_speculative",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "threads", "swa_full", "load_mode",
           "speculative_enabled"]),
        // MLX's one flag cell: whether the drafter bundle loads, and nothing else.
        ("vl_spec_decode", "mlx_macos_pipette", "mlx", &["speculative_enabled"]),
        ("vl_spec_decode", "mlx_macos_pipette", "mlx_speculative", &["speculative_enabled"]),
        // The Android engine reaches four of the six the iOS one does.
        ("prefill_throughput", "llamacpp_apk_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "load_mode"]),
        ("decode_throughput", "llamacpp_apk_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "load_mode"]),
        ("max_memory_usage", "llamacpp_apk_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "load_mode"]),
        ("end_to_end_latency", "llamacpp_apk_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "load_mode"]),
        ("eval", "llamacpp_apk_pipette", "gguf_text",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "load_mode"]),
        ("vl_throughput", "llamacpp_apk_pipette", "gguf_vision",
         &["number_gpu_layers", "ctx_size", "n_ubatch", "load_mode"]),
        // iOS MLX: prefill-chunk size only.
        ("prefill_throughput", "mlx_ios_pipette", "mlx", &["n_ubatch"]),
        ("decode_throughput", "mlx_ios_pipette", "mlx", &["n_ubatch"]),
        ("max_memory_usage", "mlx_ios_pipette", "mlx", &["n_ubatch"]),
        ("end_to_end_latency", "mlx_ios_pipette", "mlx", &["n_ubatch"]),
        ("eval", "mlx_ios_pipette", "mlx", &["n_ubatch"]),
        // OpenVINO: typed pipeline properties, no raw and no env forwards.
        ("prefill_throughput", "uv_openvino", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint"]),
        ("decode_throughput", "uv_openvino", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint"]),
        ("end_to_end_latency", "uv_openvino", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint"]),
        ("max_memory_usage", "uv_openvino", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint"]),
        // OVMS: the OpenVINO knobs plus `prefix_caching`.
        ("prefill_throughput", "ovms", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint", "prefix_caching"]),
        ("decode_throughput", "ovms", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint", "prefix_caching"]),
        ("end_to_end_latency", "ovms", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint", "prefix_caching"]),
        ("max_memory_usage", "ovms", "openvino",
         &["device", "max_prompt_len", "min_response_len", "generate_hint", "prefix_caching"]),
        // TensorRT Edge-LLM: the engine precision, env forwards and the server's own flags.
        ("vl_throughput", "trt_edgellm", "gguf_vision", &["precision", "envs", "raw"]),
        ("vl_max_memory", "trt_edgellm", "gguf_vision", &["precision", "envs", "raw"]),
    ];

    /// The [`CELLS`] table covers every variant exactly once, so the sweep
    /// below cannot silently skip one.
    #[test]
    fn cell_table_covers_every_variant() -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for (benchmark, runtime, model, _) in CELLS {
            let flags = parse(&axes_wire(benchmark, runtime, model)).map_err(|e| {
                anyhow::anyhow!("{benchmark}/{runtime}/{model} has no variant: {e}")
            })?;
            anyhow::ensure!(
                seen.insert(std::mem::discriminant(&flags)),
                "{benchmark}/{runtime}/{model} duplicates an earlier row's variant"
            );
        }
        anyhow::ensure!(
            seen.len() == RuntimeFlags::COUNT,
            "table covers {} variants, enum has {}",
            seen.len(),
            RuntimeFlags::COUNT
        );
        Ok(())
    }

    /// The JSON schema's arms are this table: one per cell, with its knobs.
    #[cfg(feature = "schemars")]
    #[test]
    fn the_json_schema_has_an_arm_per_cell_with_its_knobs() -> anyhow::Result<()> {
        crate::json_schema::assert_arms_are::<RuntimeFlags>(CELLS)
    }

    /// The JSON schema's arms are the cells the deserializer takes, each with
    /// the [`PROBES`] it takes there.
    #[cfg(feature = "schemars")]
    #[test]
    fn the_json_schema_has_an_arm_per_accepted_cell_with_its_knobs() -> anyhow::Result<()> {
        let triples = crate::cell::every_triple()
            .map(|(b, r, m)| vec![b.to_string(), r.to_string(), m.to_string()]);
        crate::json_schema::assert_arms_are_accepted::<RuntimeFlags, RuntimeFlagRef>(
            crate::json_schema::TRIPLE,
            triples,
            PROBES,
        )
    }

    /// Walks every triple the axis types allow, so no variant escapes: each is
    /// named for the one triple that routes to it, and `axes()` reports it back.
    #[test]
    fn variants_are_named_for_their_cell() -> anyhow::Result<()> {
        let mut routed = std::collections::HashSet::new();
        let mut misnamed = Vec::new();
        for (b, r, m) in crate::cell::every_triple() {
            let Ok(flags) = RuntimeFlags::try_from(RuntimeFlagRef::new(b, r, m)) else {
                continue;
            };
            anyhow::ensure!(
                flags.axes() == (b, r, m),
                "{b}/{r}/{m} routes to a variant whose axes() are {:?}",
                flags.axes()
            );
            routed.insert(std::mem::discriminant(&flags));
            let name: &str = (&flags).into();
            let expected = format!("{b:?}{r:?}{m:?}");
            if name != expected {
                misnamed.push(format!("{name} (want {expected})"));
            }
        }
        anyhow::ensure!(
            routed.len() == RuntimeFlags::COUNT,
            "triples reach {} variants, the enum has {}",
            routed.len(),
            RuntimeFlags::COUNT
        );
        anyhow::ensure!(
            misnamed.is_empty(),
            "misnamed variants: {}",
            misnamed.join(", ")
        );
        Ok(())
    }

    /// Every variant accepts exactly the fields it declares, rejects every
    /// other one, and carries the values it accepted: the arms are hand-written,
    /// so nothing structural forces it. Values are checked against the authored
    /// wire, not `parse(to_string(f)) == f`, which agrees when a field was lost.
    #[test]
    fn every_cell_accepts_exactly_its_declared_fields_and_keeps_their_values() -> anyhow::Result<()>
    {
        for (benchmark, runtime, model, accepted) in CELLS {
            for (field, value) in PROBES {
                let wire = format!(
                    "{}{field} = {value}\n",
                    axes_wire(benchmark, runtime, model)
                );
                let expected = accepted.contains(field);
                let parsed = parse(&wire);
                anyhow::ensure!(
                    parsed.is_ok() == expected,
                    "{benchmark}/{runtime}/{model}: `{field}` should be {} but was {}",
                    if expected { "accepted" } else { "rejected" },
                    if expected { "rejected" } else { "accepted" },
                );
                let Ok(flags) = parsed else { continue };
                let emitted = toml::Value::try_from(&flags)?;
                let authored = toml::from_str::<toml::Value>(&format!("{field} = {value}"))?;
                anyhow::ensure!(
                    emitted.get(field) == authored.get(field),
                    "{benchmark}/{runtime}/{model}: `{field}` was accepted but came back as {:?}, authored {:?}",
                    emitted.get(field),
                    authored.get(field),
                );
            }
        }
        Ok(())
    }
    /// The knobs only exist for OpenVINO cells. Accepting one elsewhere would
    /// report it on the submission as though it had applied to something.
    #[test]
    fn openvino_knobs_are_rejected_on_other_runtimes() -> anyhow::Result<()> {
        let mut r = RuntimeFlagRef::new(
            BenchmarkType::DecodeThroughput,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
        );
        r.min_response_len = Some(PositiveCount::try_new(256)?);
        let Err(err) = RuntimeFlags::try_from(r) else {
            anyhow::bail!("expected min_response_len to be refused on a llama.cpp cell");
        };
        assert!(err.to_string().contains("min_response_len"), "got {err}");
        Ok(())
    }

    /// A device outside the named set is the operator's to spell — an indexed
    /// GPU or a virtual device is legitimate — so it has to survive the trip a
    /// plan takes: authored text, typed flags, and back.
    #[test]
    fn an_unnamed_openvino_device_survives_the_flags() -> anyhow::Result<()> {
        let mut r = RuntimeFlagRef::new(
            BenchmarkType::EndToEndLatency,
            RuntimeType::UvOpenvino,
            ModelType::Openvino,
        );
        r.device = Some(OpenvinoDevice::Custom("GPU.1".to_owned()));

        let flags = RuntimeFlags::try_from(r)?;
        let json = serde_json::to_string(&flags)?;
        assert!(json.contains("\"device\":\"GPU.1\""), "got {json}");

        let back: RuntimeFlagRef = serde_json::from_str::<RuntimeFlags>(&json)?.into();
        assert_eq!(
            back.device,
            Some(OpenvinoDevice::Custom("GPU.1".to_owned()))
        );
        Ok(())
    }

    /// Round-trip through the flat wire form, which is how a plan authors them.
    #[test]
    fn openvino_knobs_round_trip_through_the_ref() -> anyhow::Result<()> {
        let mut r = RuntimeFlagRef::new(
            BenchmarkType::EndToEndLatency,
            RuntimeType::UvOpenvino,
            ModelType::Openvino,
        );
        r.max_prompt_len = Some(PositiveCount::try_new(1024)?);
        r.min_response_len = Some(PositiveCount::try_new(256)?);
        r.generate_hint = Some(OpenvinoGenerateHint::BestPerf);

        let flags = RuntimeFlags::try_from(r)?;
        assert_eq!(
            flags.axes(),
            (
                BenchmarkType::EndToEndLatency,
                RuntimeType::UvOpenvino,
                ModelType::Openvino
            )
        );
        let back: RuntimeFlagRef = flags.into();
        assert_eq!(back.max_prompt_len, Some(PositiveCount::try_new(1024)?));
        assert_eq!(back.min_response_len, Some(PositiveCount::try_new(256)?));
        assert_eq!(back.generate_hint, Some(OpenvinoGenerateHint::BestPerf));
        Ok(())
    }

    /// OpenVINO takes typed properties, not a command line, so there is no
    /// escape hatch and a `raw` entry has to be refused rather than ignored.
    #[test]
    fn openvino_cells_take_no_raw_entries() -> anyhow::Result<()> {
        let mut r = RuntimeFlagRef::new(
            BenchmarkType::DecodeThroughput,
            RuntimeType::UvOpenvino,
            ModelType::Openvino,
        );
        r.raw = vec!["--some-flag".to_owned()];
        let Err(err) = RuntimeFlags::try_from(r) else {
            anyhow::bail!("expected raw to be refused on an openvino cell");
        };
        assert!(err.to_string().contains("raw"), "got {err}");
        Ok(())
    }
    /// An authored pipeline override appears on the submission, since it changes
    /// what the benchmark id measures; a derived default does not.
    #[test]
    fn an_authored_override_is_visible_on_the_submission() -> anyhow::Result<()> {
        let mut r = RuntimeFlagRef::new(
            BenchmarkType::DecodeThroughput,
            RuntimeType::UvOpenvino,
            ModelType::Openvino,
        );
        r.max_prompt_len = Some(PositiveCount::try_new(2048)?);
        r.generate_hint = Some(OpenvinoGenerateHint::FastCompile);
        let submitted = RuntimeFlags::try_from(r)?.submission_value();

        assert_eq!(
            submitted.get("max_prompt_len"),
            Some(&serde_json::json!(2048))
        );
        assert_eq!(
            submitted.get("generate_hint"),
            Some(&serde_json::json!("fast-compile"))
        );
        // Not authored, so not claimed.
        assert!(submitted.get("min_response_len").is_none());

        // A cell that overrode nothing carries no properties at all, rather
        // than reporting the values the runtime derived for it.
        let bare = RuntimeFlagRef::new(
            BenchmarkType::DecodeThroughput,
            RuntimeType::UvOpenvino,
            ModelType::Openvino,
        );
        let bare = RuntimeFlags::try_from(bare)?.submission_value();
        ["max_prompt_len", "min_response_len", "generate_hint"]
            .into_iter()
            .for_each(|key| assert!(bare.get(key).is_none(), "{key} should be absent"));
        Ok(())
    }

    /// Unlike the submission record, the client runs these knobs, so env
    /// values survive; the axes go, since the client already resolved the cell.
    #[test]
    fn knobs_json_strips_the_axes_and_keeps_env_values() -> anyhow::Result<()> {
        let f = parse(&format!(
            "{}tensor_parallel_size = 2\nenvs = [\"HF_TOKEN=hunter2\", \"NCCL_DEBUG\"]\n",
            axes_wire("eval", "docker_vllm", "torch")
        ))?;
        let knobs = f.knobs_json();
        assert_eq!(
            knobs,
            serde_json::json!({
                "tensor_parallel_size": 2,
                "envs": ["HF_TOKEN=hunter2", "NCCL_DEBUG"],
            })
        );
        let back = RuntimeFlags::from_cell_json(
            &knobs.to_string(),
            RuntimeType::DockerVllm,
            ModelType::Torch,
            BenchmarkType::Eval,
        )?;
        assert_eq!(back, Some(f));
        Ok(())
    }

    /// `--runtime-flags` read against the llama.cpp CLI prefill cell.
    fn prefill_cell_json(json: &str) -> Result<Option<RuntimeFlags>, RuntimeFlagsError> {
        RuntimeFlags::from_cell_json(
            json,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
            BenchmarkType::PrefillThroughput,
        )
    }

    #[test]
    fn from_cell_json_takes_the_axes_from_the_resolved_cell() -> anyhow::Result<()> {
        let expected = parse(&format!(
            "{}threads = 4\n",
            axes_wire(
                "prefill_throughput",
                "llamacpp_cli_stock_tools",
                "gguf_text"
            )
        ))?;
        assert_eq!(prefill_cell_json(r#"{"threads":4}"#)?, Some(expected));
        // An empty knob set runs on the engine's defaults, like no flags at all.
        assert_eq!(prefill_cell_json("{}")?, None);
        Ok(())
    }

    #[rstest]
    #[case::an_array(r#"[{"threads":4}]"#, |e: &RuntimeFlagsError| {
        matches!(e, RuntimeFlagsError::NotAnObject)
    })]
    // An axis is refused even when it agrees with the resolved cell.
    #[case::runtime_axis(r#"{"runtime_type":"llamacpp_cli_stock_tools"}"#, |e: &RuntimeFlagsError| {
        matches!(e, RuntimeFlagsError::AxisNotAccepted { key: "runtime_type" })
    })]
    #[case::model_axis(r#"{"model_type":"gguf_text"}"#, |e: &RuntimeFlagsError| {
        matches!(e, RuntimeFlagsError::AxisNotAccepted { key: "model_type" })
    })]
    #[case::benchmark_axis(r#"{"benchmark_type":"prefill_throughput"}"#, |e: &RuntimeFlagsError| {
        matches!(e, RuntimeFlagsError::AxisNotAccepted { key: "benchmark_type" })
    })]
    #[case::a_knob_the_cell_refuses(r#"{"ctx_size":8192}"#, |e: &RuntimeFlagsError| {
        matches!(
            e,
            RuntimeFlagsError::KnobNotAllowed { knob: "ctx_size", .. }
        )
    })]
    #[case::a_misspelt_knob(r#"{"threadz":4}"#, |e: &RuntimeFlagsError| {
        matches!(e, RuntimeFlagsError::Json(_))
            && e.to_string().starts_with("parsing runtime flags JSON: unknown field `threadz`")
    })]
    fn from_cell_json_refusals(
        #[case] json: &str,
        #[case] expected: fn(&RuntimeFlagsError) -> bool,
    ) -> anyhow::Result<()> {
        match prefill_cell_json(json) {
            Ok(flags) => anyhow::bail!("{json} was accepted as {flags:?}"),
            Err(e) => anyhow::ensure!(expected(&e), "{json}: {e:?}"),
        }
        Ok(())
    }

    /// What a plan author or a `--runtime-flags` caller reads on a refusal.
    #[rstest]
    #[case::no_such_combination(
        RuntimeFlagsError::NoSuchCombination {
            benchmark: BenchmarkType::Eval,
            runtime: RuntimeType::AppleFoundation,
            model: ModelType::AppleFoundationText,
        }
        .to_string(),
        "no runtime flags defined for eval \u{d7} AppleFoundation \u{d7} AppleFoundationText"
    )]
    #[case::knob_not_allowed(
        RuntimeFlagsError::KnobNotAllowed {
            knob: "ctx_size",
            benchmark: BenchmarkType::PrefillThroughput,
            runtime: RuntimeType::LlamacppCliStockTools,
            model: ModelType::GgufText,
        }
        .to_string(),
        "knob `ctx_size` is not accepted by prefill_throughput \u{d7} LlamacppCliStockTools \u{d7} GgufText"
    )]
    #[case::raw_aliases_typed_knob(
        RuntimeFlagsError::RawAliasesTypedKnob { flag: "-t8".to_owned() }.to_string(),
        "raw flag \"-t8\" aliases a typed knob for this cell; set the typed field instead"
    )]
    #[case::raw_reserved_flag(
        RuntimeFlagsError::RawReservedFlag { flag: "--repetitions".to_owned() }.to_string(),
        "raw flag \"--repetitions\" is reserved by the benchmark/tool for this cell"
    )]
    #[case::raw_speculative_flag_with_speculation_off(
        RuntimeFlagsError::RawSpeculativeFlagWithSpeculationOff { flag: "--spec-type".to_owned() }
            .to_string(),
        "raw flag \"--spec-type\" configures speculation, but speculation is off for this run \
         (a model with no drafter, or speculative_enabled = false)"
    )]
    #[case::speculation_without_drafter(
        RuntimeFlagsError::SpeculationWithoutDrafter { model: ModelType::GgufVision }.to_string(),
        "speculative_enabled = true, but a gguf_vision model has no drafter; use a speculative \
         model type, or leave the flag unset"
    )]
    #[case::not_an_object(
        RuntimeFlagsError::NotAnObject.to_string(),
        "runtime flags must be a JSON object of knobs, e.g. {\"threads\":4} (the one-element \
         array is no longer accepted)"
    )]
    #[case::axis_not_accepted(
        RuntimeFlagsError::AxisNotAccepted { key: "model_type" }.to_string(),
        "runtime flags must not carry `model_type`: the cell comes from --benchmark, --runtime \
         and --model, which are parsed first"
    )]
    #[case::json(
        RuntimeFlagsError::Json("EOF while parsing a value".to_owned()).to_string(),
        "parsing runtime flags JSON: EOF while parsing a value"
    )]
    fn refusals_render_for_the_reader(#[case] rendered: String, #[case] expected: &str) {
        assert_eq!(rendered, expected);
    }

    /// A llama.cpp CLI entry converts to its cell's own type; an entry for
    /// another cell is refused by name, and the corpus type takes either
    /// model kind, the two variants it serves.
    #[test]
    fn a_llamacpp_cli_entry_converts_to_its_cells_type() -> anyhow::Result<()> {
        let cell = |benchmark, model| {
            RuntimeFlagRef::new(benchmark, RuntimeType::LlamacppCliStockTools, model)
        };

        let decode = DecodeThroughputLlamacppCliStockToolsGgufText::try_from(cell(
            BenchmarkType::DecodeThroughput,
            ModelType::GgufText,
        ))?;
        assert_eq!(decode.threads, None);

        let err = PrefillThroughputLlamacppCliStockToolsGgufText::try_from(cell(
            BenchmarkType::DecodeThroughput,
            ModelType::GgufText,
        ))
        .err()
        .ok_or_else(|| anyhow::anyhow!("a decode entry became a prefill one"))?;
        assert!(
            matches!(
                err,
                RuntimeFlagsError::NotThisCell {
                    expected: "PrefillThroughputLlamacppCliStockToolsGgufText",
                    ..
                }
            ),
            "{err}"
        );

        for model in [ModelType::GgufVision, ModelType::GgufVisionSpeculative] {
            VlSpecDecodeLlamacppCliStockTools::try_from(cell(BenchmarkType::VlSpecDecode, model))?;
        }
        Ok(())
    }

    /// An OpenVINO or OVMS entry converts to its cell's own type; the same
    /// benchmark's entry on the other runtime is refused by name.
    #[test]
    fn an_openvino_entry_converts_to_its_cells_type() -> anyhow::Result<()> {
        let cell = |benchmark, runtime| {
            let mut r = RuntimeFlagRef::new(benchmark, runtime, ModelType::Openvino);
            r.device = Some(OpenvinoDevice::Npu);
            r
        };

        let decode = DecodeThroughputOvmsOpenvino::try_from(cell(
            BenchmarkType::DecodeThroughput,
            RuntimeType::Ovms,
        ))?;
        assert_eq!(decode.device, Some(OpenvinoDevice::Npu));
        MaxMemoryUsageUvOpenvinoOpenvino::try_from(cell(
            BenchmarkType::MaxMemoryUsage,
            RuntimeType::UvOpenvino,
        ))?;

        let err = DecodeThroughputUvOpenvinoOpenvino::try_from(cell(
            BenchmarkType::DecodeThroughput,
            RuntimeType::Ovms,
        ))
        .err()
        .ok_or_else(|| anyhow::anyhow!("an OVMS entry became a uv_openvino one"))?;
        assert!(
            matches!(
                err,
                RuntimeFlagsError::NotThisCell {
                    expected: "DecodeThroughputUvOpenvinoOpenvino",
                    ..
                }
            ),
            "{err}"
        );
        Ok(())
    }
}
