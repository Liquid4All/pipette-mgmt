//! What a benchmark *is*: the per-kind parameter structs and the tagged
//! [`BenchmarkDefinition`](crate::benchmark::BenchmarkDefinition) over them, plus the catalog origin each one
//! arrived by. Pure wire vocabulary, no I/O.

mod asr_decoding;
mod asr_eval_task;
mod asr_prompt_profile;
mod asr_suite;
mod eval_id;
mod params;
mod sampling;
mod vlm_eval_recipe;
mod vlm_eval_task;
mod vlm_prompt_profile;

pub use asr_decoding::{
    AsrBeamSearch, AsrDecoding, AsrDecodingError, AsrDecodingKnob, AsrRepeatWindow,
    AsrSamplerChain, AsrSampling,
};
pub use asr_eval_task::{AsrErrorUnit, AsrEvalTask, KnownAsrEvalTask};
pub use asr_prompt_profile::{AsrPromptProfile, KnownAsrPromptProfile};
pub use asr_suite::{AsrDatasetResult, AsrSuite, AsrSuiteHeadline};
pub use eval_id::{EvalId, KnownEvalId};
pub(crate) use params::declared_benchmark_params;
pub use params::{BenchmarkEntry, BenchmarkParams, BenchmarkParamsError};
pub use sampling::{
    BeamLengthPenalty, BeamLengthPenaltyError, BeamRepetitionPenalty, BeamRepetitionPenaltyError,
    MinP, MinPError, NumBeams, NumBeamsError, PresencePenalty, PresencePenaltyError, RepeatPenalty,
    RepeatPenaltyError, SamplingSeed, SamplingSeedError, SamplingTemperature,
    SamplingTemperatureError, SeedRule, TopK, TopKError, TopP, TopPError,
};
pub use vlm_eval_recipe::{
    LanguageSubtag, LanguageSubtagError, RecipeRepo, Thinking, VlmEvalRecipe, VlmEvalRecipeError,
    VlmEvalRecipeModel,
};
pub use vlm_eval_task::{KnownVlmEvalTask, VlmEvalTask};
pub use vlm_prompt_profile::{KnownVlmPromptProfile, VlmPromptProfile};

use std::num::{NonZeroUsize, TryFromIntError};
use std::str::FromStr;

use nutype::nutype;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::{Display, VariantArray};
use thiserror::Error;

use crate::dataset::Dataset;
use crate::{EvalSample, NonNegativeCount, PositiveCount};

/// Opaque benchmark identifier as written in plan TOMLs:
/// `[A-Za-z0-9][A-Za-z0-9_.-]*`.
///
/// An id, never an address: a catalog side (`local/…`, `remote/…`) is a
/// client-local way of *finding* a definition, so a `/` is rejected.
///
/// The server interpolates ids into its catalog key and warehouse partition, so
/// the leading character is alphanumeric, never a path segment (`.`, `..`).
/// `.` is allowed for versioned ids (`eval_ifbench_2026.06.1`).
#[nutype(
    validate(with = check_benchmark_id, error = BenchmarkIdError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display,
        Into,
        TryFrom,
        FromStr
    )
)]
pub struct BenchmarkId(String);

impl BenchmarkId {
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

fn check_benchmark_id(id: &str) -> Result<(), BenchmarkIdError> {
    let mut chars = id.chars();
    let first = chars.next().ok_or(BenchmarkIdError::Empty)?;
    if !first.is_ascii_alphanumeric() {
        return Err(BenchmarkIdError::InvalidFirst(first));
    }
    match chars.find(|c| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '.' | '-')) {
        Some(ch) => Err(BenchmarkIdError::InvalidChar(ch)),
        None => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BenchmarkIdError {
    #[error("benchmark id must not be empty")]
    Empty,

    #[error("benchmark id must start with [A-Za-z0-9], not '{}'", .0.escape_default())]
    InvalidFirst(char),

    #[error("benchmark id contains '{}'; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*", .0.escape_default())]
    InvalidChar(char),
}

/// A benchmark-type string (snake_case or kebab-case) named no known
/// [`BenchmarkType`] — e.g. an unrecognized `--type` filter.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown benchmark type '{0}'")]
pub struct BenchmarkTypeError(pub String);

/// Benchmark type, also part of the `(BenchmarkType, Runtime, Model)` flag key.
/// `FromStr` also accepts the kebab-case the `--type` CLI advertises.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, VariantArray,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum BenchmarkType {
    PrefillThroughput,
    DecodeThroughput,
    EndToEndLatency,
    MaxMemoryUsage,
    Eval,
    /// A vision-language accuracy eval: an lmms-eval task over image-bearing
    /// samples, scored server-side like [`Self::Eval`].
    VlmEval,
    /// A speech-recognition accuracy eval: an Open ASR Leaderboard (English)
    /// or FLEURS set's clips, transcribed and scored server-side (word error rate)
    /// like [`Self::Eval`].
    AsrEval,
    VlThroughput,
    VlMaxMemory,
    /// One VL model over a real image + text corpus, speculating when the model
    /// has a drafter. Comparing spec and non-spec rows is a join elsewhere.
    VlSpecDecode,
    /// Speech recognition through a multimodal model's audio encoder: prefill
    /// a clip of a stated length, then decode. The audio counterpart of
    /// [`VlThroughput`](Self::VlThroughput).
    AsrThroughput,
    /// Peak memory of one audio clip through a multimodal model: the audio
    /// counterpart of [`VlMaxMemory`](Self::VlMaxMemory).
    AsrMaxMemory,
}

impl FromStr for BenchmarkType {
    type Err = BenchmarkTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Normalize kebab-case first so the `--type` CLI form parses too.
        let normalized = s.replace('-', "_");
        Self::ALL
            .iter()
            .copied()
            .find(|variant| variant.to_string() == normalized)
            .ok_or_else(|| BenchmarkTypeError(s.to_string()))
    }
}

impl BenchmarkType {
    /// All variants, in declaration order, without importing `VariantArray`.
    pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;

    /// Whether the server scores this type's results after submission.
    pub fn is_scored_via_server(self) -> bool {
        match self {
            Self::Eval | Self::VlmEval | Self::AsrEval => true,
            Self::PrefillThroughput
            | Self::AsrThroughput
            | Self::AsrMaxMemory
            | Self::DecodeThroughput
            | Self::EndToEndLatency
            | Self::MaxMemoryUsage
            | Self::VlThroughput
            | Self::VlMaxMemory
            | Self::VlSpecDecode => false,
        }
    }

    /// Infer the benchmark type from a benchmark ID by prefix matching, after an
    /// optional `local/` or `remote/`. `None` for an ID outside the known set.
    ///
    /// A heuristic: a private id may be named freely, so prefer
    /// `BenchmarkDefinition::benchmark_type()` wherever a catalog is available.
    /// Kept for readers with only the id.
    // TODO: remove once benchmark definitions are threaded through those seams.
    pub fn from_id(benchmark_id: &str) -> Option<Self> {
        let id = benchmark_id
            .strip_prefix("local/")
            .or_else(|| benchmark_id.strip_prefix("remote/"))
            .unwrap_or(benchmark_id);
        if id.starts_with("prefill_throughput") {
            Some(Self::PrefillThroughput)
        } else if id.starts_with("decode_throughput") {
            Some(Self::DecodeThroughput)
        } else if id.starts_with("end_to_end_latency") {
            Some(Self::EndToEndLatency)
        } else if id.starts_with("max_memory_usage") {
            Some(Self::MaxMemoryUsage)
        } else if id.starts_with("eval_") || id == "eval" {
            Some(Self::Eval)
        } else if id.starts_with("vlm_eval_") || id == "vlm_eval" {
            Some(Self::VlmEval)
        } else if id.starts_with("asr_eval_") || id == "asr_eval" {
            // Disjoint from the ASR timing families' `asr_throughput` and
            // `asr_max_memory` prefixes, so the branch order is free.
            Some(Self::AsrEval)
        } else if id.starts_with("vl_max_memory") {
            Some(Self::VlMaxMemory)
        } else if id.starts_with("vl_throughput") {
            Some(Self::VlThroughput)
        } else if id.starts_with("vl_spec_decode") {
            Some(Self::VlSpecDecode)
        } else if id.starts_with("asr_throughput") {
            Some(Self::AsrThroughput)
        } else if id.starts_with("asr_max_memory") {
            Some(Self::AsrMaxMemory)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// Where a benchmark definition came from: the local generated catalog, or a
/// catalog synced from the management server.
pub enum BenchmarkSource {
    Local,
    Remote,
}

/// The requests a cell keeps in flight at once; `1` when omitted. Zero is
/// refused: run as the reference procedure, it would file a serial measurement
/// under an id that promised none. At most `i32::MAX`, its warehouse column's.
#[nutype(
    validate(greater_or_equal = 1, less_or_equal = 2_147_483_647),
    default = 1,
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        Default,
        Serialize,
        Deserialize,
        Display
    )
)]
pub struct BatchSize(u32);

impl BatchSize {
    /// The reference procedure: one request in flight.
    pub fn one() -> Self {
        Self::default()
    }

    /// The bare `u32`, for an argv or a context-size product.
    pub fn as_u32(self) -> u32 {
        self.into_inner()
    }

    /// The count as its Int32 warehouse column holds it; the bound makes the
    /// fallback unreachable.
    pub fn as_i32(self) -> i32 {
        i32::try_from(self.into_inner()).unwrap_or(i32::MAX)
    }

    /// The count as the `NonZeroUsize` a spawn loop takes; fallible only in
    /// the `u32` to `usize` conversion.
    pub fn in_flight(self) -> Result<NonZeroUsize, TryFromIntError> {
        usize::try_from(self.into_inner()).and_then(NonZeroUsize::try_from)
    }
}

/// The sample rate of the clip an ASR cell sends, in Hz: from telephone audio
/// (8 kHz) to studio audio (48 kHz). Not the encoder's rate, which is the
/// runtime's to convert to.
#[nutype(
    validate(greater_or_equal = 8_000, less_or_equal = 48_000),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        Serialize,
        Deserialize,
        Display
    )
)]
pub struct AudioSampleRate(i32);

/// The length of the clip an ASR cell sends, in milliseconds: up to ten
/// minutes, past any clip a benchmark runs and before a rendered clip's
/// samples can exhaust a device's memory.
#[nutype(
    validate(greater_or_equal = 1, less_or_equal = 600_000),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        Serialize,
        Deserialize,
        Display
    )
)]
pub struct AudioDurationMillis(i32);

impl AudioDurationMillis {
    /// The bare milliseconds, for sample and token arithmetic.
    pub fn as_u32(self) -> u32 {
        self.into_inner().unsigned_abs()
    }
}

/// The tokens an ASR throughput cell decodes after its clip, N. At least two:
/// the first comes out of the prefill, so the timed decode window covers the
/// N-1 steps after it, and one token would time none. The client scales that
/// window to N steps (`asr_predicted_ms = after_first_ms / (N - 1) * N`, as
/// `vl_throughput` does), so `decode_throughput = N / asr_predicted_ms` is the
/// per-step rate at every N. An ASR peak-memory cell decodes the same N, under
/// the same bounds, so its peak is the throughput cell's workload.
///
/// At most 8192: a transcript of the longest clip a cell may send (ten
/// minutes, see [`AudioDurationMillis`]) is a few thousand tokens, and the
/// decode is part of the context the client sizes, so a larger N is a typo
/// that would otherwise fail late, on every client, at context allocation.
#[nutype(
    validate(greater_or_equal = 2, less_or_equal = 8192),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        Serialize,
        Deserialize,
        Display
    )
)]
pub struct AsrDecodeTokens(i32);

impl AsrDecodeTokens {
    /// The bare count, for a request's token limit and a context size.
    pub fn as_u32(self) -> u32 {
        self.into_inner().unsigned_abs()
    }
}

/// The channels of the clip an ASR cell sends: mono or stereo.
///
/// `u16`, unlike the other audio parameters, because that is the WAV `fmt `
/// chunk's channel field the client writes it into; the warehouse widens it
/// to `i32` losslessly.
#[nutype(
    validate(greater_or_equal = 1, less_or_equal = 2),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        Serialize,
        Deserialize,
        Display
    )
)]
pub struct AudioChannels(u16);

/// A [`BenchmarkDefinition::as_*`](BenchmarkDefinition) accessor was called for
/// the wrong kind.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("expected {expected} benchmark, got `{got_id}` ({got_type})")]
pub struct BenchmarkDefinitionError {
    pub expected: &'static str,
    pub got_id: String,
    pub got_type: String,
}

/// The three timing cells carry `parameter_batch_size`: the requests kept in
/// flight at once, each on its own slot of the one server; `1`, the reference
/// procedure, when omitted. Above `1` the reported time is the mean **per
/// request**. The llama.cpp CLI, which has no server, reports
/// `llama-batched-bench`'s one time for the batch: each stream's for decode,
/// and for prefill each prompt's within one `--batch-size`, the slowest
/// prompt's above it. A parameter, not a cell knob, so the id carries it
/// (`_bs<n>`) and rows at different concurrency cannot collide under one id. An
/// engine that runs one sequence at a time refuses a batched entry rather than
/// filing a serial measurement under the batched id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefillThroughputBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_prefill_tokens: NonNegativeCount,
    #[serde(default)]
    pub parameter_batch_size: BatchSize,
}

/// See [`PrefillThroughputBenchmark`] for `parameter_batch_size`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeThroughputBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_prefill_tokens: NonNegativeCount,
    pub parameter_decode_tokens: PositiveCount,
    #[serde(default)]
    pub parameter_batch_size: BatchSize,
}

/// See [`PrefillThroughputBenchmark`] for `parameter_batch_size`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndToEndLatencyBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_prefill_tokens: NonNegativeCount,
    pub parameter_decode_tokens: PositiveCount,
    #[serde(default)]
    pub parameter_batch_size: BatchSize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaxMemoryUsageBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_prefill_tokens: NonNegativeCount,
}

/// Every sampling knob is optional: an entry that omits one leaves the client's
/// default in place, and one that pins any is its own benchmark id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_eval_id: EvalId,
    pub parameter_dataset_name: String,
    pub parameter_max_tokens: PositiveCount,
    /// Where a sample's prompt plus output stops. A parameter, not a runtime
    /// flag, because it changes what is measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_max_context: Option<PositiveCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_mcq_choices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_temperature: Option<SamplingTemperature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_top_p: Option<TopP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_top_k: Option<TopK>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_min_p: Option<MinP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_seed: Option<SamplingSeed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<Value>>,
}

impl EvalBenchmark {
    /// Sampling temperature for `/completion`: the catalog's when the entry
    /// pins one, else the client-side policy keyed on the eval id.
    pub fn sampling_temperature(&self) -> Result<SamplingTemperature, SamplingTemperatureError> {
        match self.parameter_temperature {
            Some(temperature) => Ok(temperature),
            None => SamplingTemperature::try_new(self.parameter_eval_id.sampling_temperature()),
        }
    }

    /// Seed to send with `sample_id`, or `None` when the entry pins none. Repeats
    /// (`<id>#k`) are independent draws only with distinct seeds, so the repeat
    /// index offsets the base seed. Lives here so every client shares the rule.
    pub fn sample_seed(&self, sample_id: &str) -> Option<i64> {
        self.parameter_seed.map(|seed| repeat_seed(seed, sample_id))
    }

    /// Whether `parameter_max_context` can be the bound that binds; the
    /// catalog loader refuses an entry that fails it.
    pub fn check_max_context(&self) -> Result<(), MaxContextError> {
        let Some(max_context) = self.parameter_max_context else {
            return Ok(());
        };
        if self.parameter_mcq_choices.is_some() {
            return Err(MaxContextError::WithMcqChoices);
        }
        if max_context > self.parameter_max_tokens {
            return Err(MaxContextError::AboveMaxTokens {
                max_context,
                max_tokens: self.parameter_max_tokens,
            });
        }
        Ok(())
    }
}

/// The seed `sample_id` is sent with, offset from `seed` by its repeat index:
/// [`EvalBenchmark::sample_seed`]'s rule, and a vlm_eval or asr_eval entry's
/// that names no [`SeedRule`].
fn repeat_seed(seed: SamplingSeed, sample_id: &str) -> i64 {
    // A repeat index is a count, so a suffix that is anything else (`#draft`,
    // `#-5`) leaves the base seed alone rather than moving it.
    let repeat = sample_id
        .rsplit_once('#')
        .and_then(|(_, k)| k.parse::<u32>().ok())
        .map_or(0, i64::from);
    // Wraps inside the cap, so the derived seed is one a runtime can carry
    // and `k` distinct repeats stay `k` distinct seeds.
    (seed.into_inner() + repeat) % (i64::from(u32::MAX) + 1)
}

/// The seed `sample_id` is sent with under `rule`: the repeat offset when
/// the entry names none, as every vlm_eval and asr_eval began, else
/// [`per_sample_seed`].
fn derive_seed(seed: SamplingSeed, rule: Option<SeedRule>, sample_id: &str) -> i64 {
    match rule {
        None => repeat_seed(seed, sample_id),
        Some(SeedRule::PerSampleSha256) => per_sample_seed(seed, sample_id),
    }
}

/// [`SeedRule::PerSampleSha256`]: the first four bytes of
/// `sha256("<seed>|<sample_id>")` in UTF-8, big-endian, a `uint32_t` every
/// runtime can carry. Each sample of the dataset, and each repeat `<id>#k`,
/// draws with its own seed, and a re-run draws each one again exactly.
///
/// The repeat offset sends every first draw the same seed. vLLM turns a
/// request's seed into a noise vector over the vocabulary, so one seed for
/// every request favours the same token ids across the whole dataset
/// (MiniCPM-V 4.5's POPE F1 fell to 55.8 that way on vLLM, 87.0 with a seed per
/// sample).
fn per_sample_seed(seed: SamplingSeed, sample_id: &str) -> i64 {
    use sha2::Digest as _;
    let digest: [u8; 32] =
        sha2::Sha256::digest(format!("{}|{sample_id}", seed.into_inner())).into();
    let [a, b, c, d, ..] = digest;
    i64::from(u32::from_be_bytes([a, b, c, d]))
}

/// Why [`EvalBenchmark::check_max_context`] refuses an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MaxContextError {
    #[error(
        "parameter_max_context ({max_context}) exceeds parameter_max_tokens ({max_tokens}), \
         but it must be the bound that binds: vLLM enforces it by sending no max_tokens. Set it \
         at or under parameter_max_tokens"
    )]
    AboveMaxTokens {
        max_context: PositiveCount,
        max_tokens: PositiveCount,
    },
    #[error(
        "parameter_max_context is set beside parameter_mcq_choices, whose few-token answers no \
         context bound reaches; drop one of the two"
    )]
    WithMcqChoices,
}

/// A vision-language eval: the eval harness applied to image-bearing samples.
/// Keyed off an lmms-eval task + dataset version rather than an [`EvalId`],
/// since VLM datasets are resolved from the scoring service's dataset store.
/// A catalog entry pins its samples as a `vlm-eval-corpus/1` [`Dataset`] a
/// client downloads; a client's own local definition carries them inline in
/// `samples` instead. Mirrors the clients' plan-types.
///
/// Every sampling knob is optional, so an entry can follow a model card's
/// decoding. An entry that pins none decodes greedily with every other sampler
/// neutral; one that pins any is its own benchmark id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlmEvalBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_lmms_task: VlmEvalTask,
    pub parameter_dataset_version: String,
    pub parameter_sample_count: PositiveCount,
    pub parameter_max_tokens: PositiveCount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_mcq_choices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_temperature: Option<SamplingTemperature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_top_p: Option<TopP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_top_k: Option<TopK>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_min_p: Option<MinP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_presence_penalty: Option<PresencePenalty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_repeat_penalty: Option<RepeatPenalty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_seed: Option<SamplingSeed>,
    /// How each sample's seed derives from `parameter_seed` ([`SeedRule`]);
    /// absent, the repeat-offset rule. Only meaningful with a seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_seed_rule: Option<SeedRule>,
    /// The model family whose VLMEvalKit wording the evals server gives the
    /// samples' prompts; absent, every model gets the dataset's own prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_prompt_profile: Option<VlmPromptProfile>,
    /// The visual tokens every image is given, for a family whose processor
    /// takes a token budget (Gemma 4's `max_soft_tokens`): the runtime sizes
    /// each image to it, as the family's own processor does. Absent, the
    /// runtime's default, which for some families sizes a small image far
    /// below the budget the model was evaluated with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_image_tokens: Option<PositiveCount>,
    /// The packed samples a client downloads: the evals server's for this
    /// task, version and profile, each image stored once. Every catalog entry
    /// pins one; absent only on a local definition, which carries `samples`.
    /// Boxed: a pin is several times the size of the rest of the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<Box<Dataset>>,
    /// The models these settings are for, their thinking mode, and the public
    /// sources the settings come from. Every catalog entry carries one; a
    /// cell outside it is refused. Not a parameter: it decides who may run
    /// the entry, not what a run measures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<VlmEvalRecipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<EvalSample>>,
}

impl VlmEvalBenchmark {
    /// The entry's temperature, else greedy: unlike [`EvalBenchmark`], no
    /// client-side policy fills the gap.
    pub fn sampling_temperature(&self) -> SamplingTemperature {
        self.parameter_temperature.unwrap_or_default()
    }

    /// Seed to send with `sample_id`, or `None` when the entry pins none, by
    /// the entry's [`SeedRule`] ([`derive_seed`]).
    pub fn sample_seed(&self, sample_id: &str) -> Option<i64> {
        self.parameter_seed
            .map(|seed| derive_seed(seed, self.parameter_seed_rule, sample_id))
    }
}

/// A speech-recognition accuracy eval: one Open ASR Leaderboard (English) or
/// FLEURS set's clips, transcribed and scored by the evals server with the Open
/// ASR Leaderboard's protocols, its English one for the English sets and its
/// multilingual one for FLEURS (word error rate). Keyed off the server's
/// dataset id + version, as [`VlmEvalBenchmark`] is.
///
/// The model family's ASR recipe (its system and user turns, Voxtral's
/// transcription template arguments) is the evals server's, picked by the
/// required [`Self::parameter_prompt_profile`]; the decoding is this entry's.
/// There is no thinking knob: an ASR model never reasons, and every runtime runs
/// it with thinking off. The samples are never attached to the entry (an English
/// set is gigabytes of audio): a device downloads them as the entry's pinned
/// [`Self::dataset`], an `asr-eval-corpus/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrEvalBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_asr_task: AsrEvalTask,
    pub parameter_dataset_version: String,
    pub parameter_sample_count: PositiveCount,
    pub parameter_prompt_profile: AsrPromptProfile,
    pub parameter_max_tokens: PositiveCount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_temperature: Option<SamplingTemperature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_top_p: Option<TopP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_top_k: Option<TopK>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_min_p: Option<MinP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_repeat_penalty: Option<RepeatPenalty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_seed: Option<SamplingSeed>,
    /// How each sample's seed derives from `parameter_seed` ([`SeedRule`]);
    /// absent, the repeat-offset rule. Only meaningful with a seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_seed_rule: Option<SeedRule>,
    /// Beam search instead of sampling (MiniCPM-o's OpenBMB protocol: 3, as
    /// OmniEvalKit's `generation_configs.json` sets it), on the llama-server
    /// of pipette's ASR build of llama.cpp (`n_beams`). Another width is a
    /// deviation from that recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_num_beams: Option<NumBeams>,
    /// Unset is the card's: MiniCPM-o's `1.2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_beam_repetition_penalty: Option<BeamRepetitionPenalty>,
    /// Unset is the card's: MiniCPM-o's `1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_beam_length_penalty: Option<BeamLengthPenalty>,
    /// The packed samples a client downloads: the evals server's for this
    /// task, version and profile, each clip stored once. Every catalog entry
    /// pins one. Boxed, as on [`VlmEvalBenchmark`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<Box<Dataset>>,
    /// The models these settings are for, and the public sources they come
    /// from, as on [`VlmEvalBenchmark`]. Every catalog entry carries one, with
    /// `enable_thinking = false`; a cell outside it is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<VlmEvalRecipe>,
}

impl AsrEvalBenchmark {
    /// As [`VlmEvalBenchmark::sample_seed`].
    pub fn sample_seed(&self, sample_id: &str) -> Option<i64> {
        self.parameter_seed
            .map(|seed| derive_seed(seed, self.parameter_seed_rule, sample_id))
    }

    /// Whether the task's language is one the recipe's card declares for
    /// speech recognition ([`VlmEvalRecipe::languages`]): `Some(false)` marks
    /// a cell outside the card's declared support, which a report flags and
    /// leaves out of any average over supported cells. `None` when the recipe
    /// enumerates no languages, or the task is one this build does not know.
    pub fn within_declared_languages(&self) -> Option<bool> {
        let language = self.parameter_asr_task.language()?;
        self.recipe.as_ref()?.declares(language)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlThroughputBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_image_width: PositiveCount,
    pub parameter_image_height: PositiveCount,
    pub parameter_text_tokens: NonNegativeCount,
    pub parameter_decode_tokens: PositiveCount,
    /// Images in the prompt (>1 = multi-frame/video); defaults to 1.
    #[serde(default = "default_num_images")]
    pub parameter_num_images: NonNegativeCount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlMaxMemoryBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_image_width: PositiveCount,
    pub parameter_image_height: PositiveCount,
    #[serde(default)]
    pub parameter_text_tokens: NonNegativeCount,
    /// Output tokens decoded after the prompt; absent is one token, and an
    /// entry without it keeps the body and params it had before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_decode_tokens: Option<PositiveCount>,
    #[serde(default = "default_num_images")]
    pub parameter_num_images: NonNegativeCount,
}

/// One VL model over a real image + text corpus: the DSpark speculative
/// decoding benchmark.
///
/// The run's `speculative_enabled` runtime flag decides the mode, defaulting
/// to the model's. Both modes file under this one id, so a comparison's two
/// rows share the corpus and draft length; the comparison is a join elsewhere.
///
/// The drafter's identity lives in the run's [`Model`](crate::Model); the
/// draft length is a parameter because it changes the measurement on a fixed
/// model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlSpecDecodeBenchmark {
    pub benchmark_id: BenchmarkId,
    /// Tokens the drafter proposes per verification step (`k`). Applies only
    /// to a spec-decoding model.
    pub parameter_draft_length: PositiveCount,
    /// Generation cap per turn.
    pub parameter_max_tokens: PositiveCount,
    /// Samples kept in flight at once, each on its own slot of the one
    /// server; `1`, the reference procedure, when omitted. A parameter, not a
    /// cell knob, so the id carries it (`_bs<n>`) and rows at different
    /// concurrency cannot collide under one id.
    #[serde(default)]
    pub parameter_batch_size: BatchSize,
    /// Sampling temperature per turn; greedy, the reference procedure, when
    /// omitted. A parameter because sampling moves the acceptance rate, and the
    /// greedy-identity gate between a spec and a non-spec row holds only at `0.0`.
    #[serde(default)]
    pub parameter_temperature: SamplingTemperature,
    /// The corpus the traversal walks, which the client downloads from mgmt
    /// and deletes when the job ends. See [`crate::dataset`].
    pub dataset: Dataset,
}

/// Audio in, text out: one audio clip of `parameter_audio_duration_ms` and
/// `parameter_text_tokens` of text beside it, then `parameter_decode_tokens`
/// generated after them.
///
/// The clip is a synthetic tone, 16-bit PCM WAV at the stated rate and channel
/// count: how many audio tokens a clip becomes depends on its length alone,
/// never its content, so the workload is fixed by the duration. The runner
/// reports the count beside the timings, and how it got it
/// ([`AsrAudioTokensSource`](crate::AsrAudioTokensSource)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrThroughputBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_audio_duration_ms: AudioDurationMillis,
    pub parameter_audio_sample_rate_hz: AudioSampleRate,
    pub parameter_audio_channels: AudioChannels,
    /// Text tokens in the user turn beside the clip and the model's own ask
    /// (instructions or tool schemas an app sends with the audio), as
    /// [`VlThroughputBenchmark::parameter_text_tokens`] is beside an image;
    /// `0`, the clip and the ask alone, when omitted.
    #[serde(default)]
    pub parameter_text_tokens: NonNegativeCount,
    pub parameter_decode_tokens: AsrDecodeTokens,
}

/// Peak memory of one audio clip and the text beside it: model and projector
/// load, the clip through the audio encoder, then `parameter_decode_tokens`
/// decoded. Clip, text and decode match [`AsrThroughputBenchmark`]'s, so the
/// peak includes the KV cache the output fills. The client sizes the context
/// from the clip's exact audio-token count where it can, else an upper bound
/// or the cell's pinned `ctx_size`, and records which
/// ([`AsrContextSizing`](crate::AsrContextSizing)); only an `exact` row is
/// the clip's own peak, with no KV cache for audio that is not there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrMaxMemoryBenchmark {
    pub benchmark_id: BenchmarkId,
    pub parameter_audio_duration_ms: AudioDurationMillis,
    pub parameter_audio_sample_rate_hz: AudioSampleRate,
    pub parameter_audio_channels: AudioChannels,
    /// Text tokens beside the clip, as in
    /// [`AsrThroughputBenchmark::parameter_text_tokens`]; the context holds
    /// them, so they are part of the peak. `0` when omitted.
    #[serde(default)]
    pub parameter_text_tokens: NonNegativeCount,
    /// Tokens decoded after the prompt, as in
    /// [`AsrThroughputBenchmark::parameter_decode_tokens`]; the context holds
    /// them, so they are part of the peak. Required, as there.
    pub parameter_decode_tokens: AsrDecodeTokens,
}

#[expect(clippy::expect_used, reason = "1 is in range")]
fn default_num_images() -> NonNegativeCount {
    NonNegativeCount::try_new(1).expect("1 is non-negative")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "benchmark_type", rename_all = "snake_case")]
pub enum BenchmarkDefinition {
    PrefillThroughput(PrefillThroughputBenchmark),
    DecodeThroughput(DecodeThroughputBenchmark),
    EndToEndLatency(EndToEndLatencyBenchmark),
    MaxMemoryUsage(MaxMemoryUsageBenchmark),
    Eval(EvalBenchmark),
    VlmEval(VlmEvalBenchmark),
    AsrEval(AsrEvalBenchmark),
    VlThroughput(VlThroughputBenchmark),
    VlMaxMemory(VlMaxMemoryBenchmark),
    VlSpecDecode(VlSpecDecodeBenchmark),
    AsrThroughput(AsrThroughputBenchmark),
    AsrMaxMemory(AsrMaxMemoryBenchmark),
}

impl BenchmarkDefinition {
    pub fn benchmark_id(&self) -> &BenchmarkId {
        match self {
            Self::PrefillThroughput(b) => &b.benchmark_id,
            Self::DecodeThroughput(b) => &b.benchmark_id,
            Self::EndToEndLatency(b) => &b.benchmark_id,
            Self::MaxMemoryUsage(b) => &b.benchmark_id,
            Self::Eval(b) => &b.benchmark_id,
            Self::VlmEval(b) => &b.benchmark_id,
            Self::AsrEval(b) => &b.benchmark_id,
            Self::VlThroughput(b) => &b.benchmark_id,
            Self::VlMaxMemory(b) => &b.benchmark_id,
            Self::VlSpecDecode(b) => &b.benchmark_id,
            Self::AsrThroughput(b) => &b.benchmark_id,
            Self::AsrMaxMemory(b) => &b.benchmark_id,
        }
    }

    pub fn benchmark_type(&self) -> BenchmarkType {
        match self {
            Self::PrefillThroughput(_) => BenchmarkType::PrefillThroughput,
            Self::DecodeThroughput(_) => BenchmarkType::DecodeThroughput,
            Self::EndToEndLatency(_) => BenchmarkType::EndToEndLatency,
            Self::MaxMemoryUsage(_) => BenchmarkType::MaxMemoryUsage,
            Self::Eval(_) => BenchmarkType::Eval,
            Self::VlmEval(_) => BenchmarkType::VlmEval,
            Self::AsrEval(_) => BenchmarkType::AsrEval,
            Self::VlThroughput(_) => BenchmarkType::VlThroughput,
            Self::VlMaxMemory(_) => BenchmarkType::VlMaxMemory,
            Self::VlSpecDecode(_) => BenchmarkType::VlSpecDecode,
            Self::AsrThroughput(_) => BenchmarkType::AsrThroughput,
            Self::AsrMaxMemory(_) => BenchmarkType::AsrMaxMemory,
        }
    }

    /// An eval's, vlm_eval's or asr_eval's sampling temperature, or greedy `0.0`
    /// for the other kinds.
    pub fn eval_temperature(&self) -> Result<SamplingTemperature, SamplingTemperatureError> {
        match self {
            Self::Eval(b) => b.sampling_temperature(),
            Self::VlmEval(b) => Ok(b.sampling_temperature()),
            Self::AsrEval(b) => Ok(b.sampling_temperature()),
            Self::PrefillThroughput(_)
            | Self::AsrThroughput(_)
            | Self::AsrMaxMemory(_)
            | Self::DecodeThroughput(_)
            | Self::EndToEndLatency(_)
            | Self::MaxMemoryUsage(_)
            | Self::VlThroughput(_)
            | Self::VlMaxMemory(_)
            | Self::VlSpecDecode(_) => Ok(SamplingTemperature::greedy()),
        }
    }

    fn unexpected_type(&self, expected: &'static str) -> BenchmarkDefinitionError {
        BenchmarkDefinitionError {
            expected,
            got_id: self.benchmark_id().to_string(),
            got_type: self.benchmark_type().to_string(),
        }
    }

    pub fn as_prefill_throughput(
        &self,
    ) -> Result<&PrefillThroughputBenchmark, BenchmarkDefinitionError> {
        let Self::PrefillThroughput(b) = self else {
            return Err(self.unexpected_type("prefill_throughput"));
        };
        Ok(b)
    }

    pub fn as_decode_throughput(
        &self,
    ) -> Result<&DecodeThroughputBenchmark, BenchmarkDefinitionError> {
        let Self::DecodeThroughput(b) = self else {
            return Err(self.unexpected_type("decode_throughput"));
        };
        Ok(b)
    }

    pub fn as_end_to_end_latency(
        &self,
    ) -> Result<&EndToEndLatencyBenchmark, BenchmarkDefinitionError> {
        let Self::EndToEndLatency(b) = self else {
            return Err(self.unexpected_type("end_to_end_latency"));
        };
        Ok(b)
    }

    pub fn as_max_memory_usage(
        &self,
    ) -> Result<&MaxMemoryUsageBenchmark, BenchmarkDefinitionError> {
        let Self::MaxMemoryUsage(b) = self else {
            return Err(self.unexpected_type("max_memory_usage"));
        };
        Ok(b)
    }

    pub fn as_eval(&self) -> Result<&EvalBenchmark, BenchmarkDefinitionError> {
        let Self::Eval(b) = self else {
            return Err(self.unexpected_type("eval"));
        };
        Ok(b)
    }

    pub fn as_vlm_eval(&self) -> Result<&VlmEvalBenchmark, BenchmarkDefinitionError> {
        let Self::VlmEval(b) = self else {
            return Err(self.unexpected_type("vlm_eval"));
        };
        Ok(b)
    }

    pub fn as_asr_eval(&self) -> Result<&AsrEvalBenchmark, BenchmarkDefinitionError> {
        let Self::AsrEval(b) = self else {
            return Err(self.unexpected_type("asr_eval"));
        };
        Ok(b)
    }

    pub fn as_vl_max_memory(&self) -> Result<&VlMaxMemoryBenchmark, BenchmarkDefinitionError> {
        let Self::VlMaxMemory(b) = self else {
            return Err(self.unexpected_type("vl_max_memory"));
        };
        Ok(b)
    }

    pub fn as_vl_throughput(&self) -> Result<&VlThroughputBenchmark, BenchmarkDefinitionError> {
        let Self::VlThroughput(b) = self else {
            return Err(self.unexpected_type("vl_throughput"));
        };
        Ok(b)
    }

    pub fn as_vl_spec_decode(&self) -> Result<&VlSpecDecodeBenchmark, BenchmarkDefinitionError> {
        let Self::VlSpecDecode(b) = self else {
            return Err(self.unexpected_type("vl_spec_decode"));
        };
        Ok(b)
    }

    pub fn as_asr_max_memory(&self) -> Result<&AsrMaxMemoryBenchmark, BenchmarkDefinitionError> {
        let Self::AsrMaxMemory(b) = self else {
            return Err(self.unexpected_type("asr_max_memory"));
        };
        Ok(b)
    }

    pub fn as_asr_throughput(&self) -> Result<&AsrThroughputBenchmark, BenchmarkDefinitionError> {
        let Self::AsrThroughput(b) = self else {
            return Err(self.unexpected_type("asr_throughput"));
        };
        Ok(b)
    }

    /// The corpus this benchmark pulls from mgmt before it runs; `None` for
    /// every kind that pulls none. On the definition so a client need not know
    /// which kinds serve one.
    pub fn dataset(&self) -> Option<&Dataset> {
        match self {
            Self::VlSpecDecode(b) => Some(&b.dataset),
            Self::VlmEval(b) => b.dataset.as_deref(),
            Self::AsrEval(b) => b.dataset.as_deref(),
            Self::PrefillThroughput(_)
            | Self::AsrThroughput(_)
            | Self::AsrMaxMemory(_)
            | Self::DecodeThroughput(_)
            | Self::EndToEndLatency(_)
            | Self::MaxMemoryUsage(_)
            | Self::Eval(_)
            | Self::VlThroughput(_)
            | Self::VlMaxMemory(_) => None,
        }
    }

    /// [`Self::dataset`], for the server to stamp the corpus `url` it serves.
    pub fn dataset_mut(&mut self) -> Option<&mut Dataset> {
        match self {
            Self::VlSpecDecode(b) => Some(&mut b.dataset),
            Self::VlmEval(b) => b.dataset.as_deref_mut(),
            Self::AsrEval(b) => b.dataset.as_deref_mut(),
            Self::PrefillThroughput(_)
            | Self::AsrThroughput(_)
            | Self::AsrMaxMemory(_)
            | Self::DecodeThroughput(_)
            | Self::EndToEndLatency(_)
            | Self::MaxMemoryUsage(_)
            | Self::Eval(_)
            | Self::VlThroughput(_)
            | Self::VlMaxMemory(_) => None,
        }
    }

    /// The requests this benchmark keeps in flight at once; `None` for kinds
    /// with no batch. On the definition, like [`Self::dataset`], so an engine
    /// can ask before it loads anything.
    pub fn batch_size(&self) -> Option<BatchSize> {
        match self {
            Self::PrefillThroughput(b) => Some(b.parameter_batch_size),
            Self::DecodeThroughput(b) => Some(b.parameter_batch_size),
            Self::EndToEndLatency(b) => Some(b.parameter_batch_size),
            Self::VlSpecDecode(b) => Some(b.parameter_batch_size),
            Self::MaxMemoryUsage(_)
            | Self::Eval(_)
            | Self::VlmEval(_)
            | Self::AsrEval(_)
            | Self::VlThroughput(_)
            | Self::VlMaxMemory(_)
            | Self::AsrThroughput(_)
            | Self::AsrMaxMemory(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::dataset::mmspec_corpus;

    /// A leading `.` would make the id a relative path segment, and a leading `-`
    /// reads as a flag to anything shelling out.
    #[rstest::rstest]
    #[case("prefill_throughput_256", true)]
    #[case("eval_ifbench_2026.06.1", true)]
    #[case("vl_spec_decode_mmspec_test_gqa_k8_bs8", true)]
    #[case("2026_first_char_may_be_a_digit", true)]
    #[case("", false)]
    #[case("   ", false)]
    #[case(".", false)]
    #[case("..", false)]
    #[case(".hidden", false)]
    #[case("-flag", false)]
    #[case("_leading_underscore", false)]
    #[case("remote/eval_smoke", false)]
    #[case("a:b", false)]
    #[case("has space", false)]
    fn benchmark_id_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(BenchmarkId::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
        // The wire holds an id to the same grammar, and keeps an accepted one verbatim.
        let parsed = serde_json::from_value::<BenchmarkId>(serde_json::Value::from(s));
        assert_eq!(parsed.is_ok(), ok, "{s:?}");
        if let Ok(id) = parsed {
            assert_eq!(id.as_str(), s);
        }
    }

    /// The leading character has its own rule and its own refusal, and a refused
    /// char is escaped even when printable, so a look-alike cannot pass for ASCII.
    #[rstest::rstest]
    #[case::inner(
        "a:b",
        "benchmark id contains ':'; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*"
    )]
    #[case::leading(".hidden", "benchmark id must start with [A-Za-z0-9], not '.'")]
    #[case::leading_look_alike(
        "\u{430}b",
        "benchmark id must start with [A-Za-z0-9], not '\\u{430}'"
    )]
    #[case::inner_look_alike(
        "b\u{430}",
        "benchmark id contains '\\u{430}'; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*"
    )]
    #[case::empty("", "benchmark id must not be empty")]
    fn a_refused_benchmark_id_names_the_character(#[case] id: &str, #[case] rendered: &str) {
        assert_eq!(
            BenchmarkId::try_new(id.to_owned())
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some(rendered)
        );
    }

    #[test]
    fn from_str_round_trips_every_variant() {
        BenchmarkType::ALL.iter().for_each(|&variant| {
            assert_eq!(
                variant.to_string().parse::<BenchmarkType>().ok(),
                Some(variant)
            );
        });
    }

    #[test]
    fn from_str_accepts_snake_and_kebab() -> Result<(), BenchmarkTypeError> {
        assert_eq!(
            "prefill_throughput".parse::<BenchmarkType>()?,
            BenchmarkType::PrefillThroughput
        );
        // The kebab spelling the `--type` CLI advertises parses too.
        assert_eq!(
            "prefill-throughput".parse::<BenchmarkType>()?,
            BenchmarkType::PrefillThroughput
        );
        assert!("mystery_metric".parse::<BenchmarkType>().is_err());
        Ok(())
    }

    #[test]
    fn from_id_tolerates_unknown_as_none() {
        assert_eq!(
            BenchmarkType::from_id("prefill_throughput_256"),
            Some(BenchmarkType::PrefillThroughput)
        );
        assert_eq!(
            BenchmarkType::from_id("remote/eval_mmlu"),
            Some(BenchmarkType::Eval)
        );
        // A type the client doesn't implement is tolerated, not an error.
        assert_eq!(BenchmarkType::from_id("mystery_metric_1"), None);
    }

    /// The clip's format is part of the definition: an entry that leaves either
    /// field out, or names a rate or channel count outside the range, is refused.
    #[rstest]
    #[case::mono_16k(16_000, 1, true)]
    #[case::stereo_48k(48_000, 2, true)]
    #[case::telephone(8_000, 1, true)]
    #[case::rate_below(7_999, 1, false)]
    #[case::rate_above(48_001, 1, false)]
    #[case::no_channels(16_000, 0, false)]
    #[case::surround(16_000, 6, false)]
    fn an_asr_clip_format_is_bounded(#[case] rate: i32, #[case] channels: u16, #[case] ok: bool) {
        let definition = serde_json::json!({
            "benchmark_type": "asr_max_memory",
            "benchmark_id": "asr_max_memory_9400ms_text0_decode256",
            "parameter_audio_duration_ms": 9400,
            "parameter_audio_sample_rate_hz": rate,
            "parameter_audio_channels": channels,
            "parameter_decode_tokens": 256,
        });
        assert_eq!(
            serde_json::from_value::<BenchmarkDefinition>(definition).is_ok(),
            ok,
            "{rate} Hz, {channels} channels"
        );
    }

    /// Both ASR types hold [`AsrDecodeTokens`]'s bounds, refused at catalog load.
    #[rstest]
    #[case::one(1, false)]
    #[case::two(2, true)]
    #[case::catalog(256, true)]
    #[case::the_bound(8192, true)]
    #[case::past_the_bound(8193, false)]
    #[case::i32_max(i32::MAX, false)]
    fn an_asr_decode_length_is_bounded(
        #[case] decode_tokens: i32,
        #[case] ok: bool,
        #[values("asr_throughput", "asr_max_memory")] benchmark_type: &str,
    ) {
        let definition = serde_json::json!({
            "benchmark_type": benchmark_type,
            "benchmark_id": format!("{benchmark_type}_9400ms_text0_decode256"),
            "parameter_audio_duration_ms": 9400,
            "parameter_audio_sample_rate_hz": 16_000,
            "parameter_audio_channels": 1,
            "parameter_decode_tokens": decode_tokens,
        });
        assert_eq!(
            serde_json::from_value::<BenchmarkDefinition>(definition).is_ok(),
            ok,
            "{benchmark_type}: {decode_tokens} decode tokens"
        );
    }

    /// The clip's format and the decode count have no default on either ASR
    /// type: a definition without one is refused at load.
    #[rstest]
    fn an_asr_definition_requires_its_clip_format_and_decode(
        #[values(
            "parameter_audio_sample_rate_hz",
            "parameter_audio_channels",
            "parameter_decode_tokens"
        )]
        field: &str,
        #[values("asr_throughput", "asr_max_memory")] benchmark_type: &str,
    ) {
        let mut definition = serde_json::json!({
            "benchmark_type": benchmark_type,
            "benchmark_id": format!("{benchmark_type}_9400ms_text0_decode128"),
            "parameter_audio_duration_ms": 9400,
            "parameter_audio_sample_rate_hz": 16_000,
            "parameter_audio_channels": 1,
            "parameter_decode_tokens": 128,
        });
        if let Some(fields) = definition.as_object_mut() {
            fields.remove(field);
        }
        let err = serde_json::from_value::<BenchmarkDefinition>(definition)
            .err()
            .map(|e| e.to_string());
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains(&format!("missing field `{field}`"))),
            "{err:?}"
        );
    }

    // `vl_max_memory` must not be swallowed by the `vl_throughput` prefix arm,
    // nor either ASR prefix by the other.
    #[rstest]
    #[case("prefill_throughput_256", Some(BenchmarkType::PrefillThroughput))]
    #[case("decode_throughput_512_100", Some(BenchmarkType::DecodeThroughput))]
    #[case("end_to_end_latency_8192_256", Some(BenchmarkType::EndToEndLatency))]
    #[case("max_memory_usage_4096", Some(BenchmarkType::MaxMemoryUsage))]
    #[case("eval_ifbench_2026.06.1", Some(BenchmarkType::Eval))]
    #[case("vl_max_memory_256x256_text0_img1", Some(BenchmarkType::VlMaxMemory))]
    #[case(
        "vl_throughput_512x512_text0_decode64_img1",
        Some(BenchmarkType::VlThroughput)
    )]
    #[case(
        "local/vl_max_memory_512x512_text1024_img1",
        Some(BenchmarkType::VlMaxMemory)
    )]
    #[case("vl_spec_decode_mmspec_test_gqa_k8", Some(BenchmarkType::VlSpecDecode))]
    #[case(
        "remote/vl_spec_decode_mmspec_test_gqa_k8",
        Some(BenchmarkType::VlSpecDecode)
    )]
    #[case(
        "asr_eval_esb_librispeech_test_clean_2026.10.1_qwen25_omni",
        Some(BenchmarkType::AsrEval)
    )]
    #[case(
        "remote/asr_eval_fleurs_nl_nl_2026.10.1_voxtral",
        Some(BenchmarkType::AsrEval)
    )]
    #[case("asr_evaluation", None)]
    #[case(
        "asr_throughput_9400ms_text0_decode128",
        Some(BenchmarkType::AsrThroughput)
    )]
    #[case(
        "remote/asr_throughput_2500ms_text1024_decode256",
        Some(BenchmarkType::AsrThroughput)
    )]
    #[case(
        "remote/asr_throughput_119900ms_text0_decode256",
        Some(BenchmarkType::AsrThroughput)
    )]
    #[case(
        "asr_max_memory_29900ms_text1024_decode256",
        Some(BenchmarkType::AsrMaxMemory)
    )]
    #[case(
        "remote/asr_max_memory_119900ms_text1024_decode256",
        Some(BenchmarkType::AsrMaxMemory)
    )]
    #[case(
        "asr_max_memory_2500ms_text0_decode256",
        Some(BenchmarkType::AsrMaxMemory)
    )]
    #[case("local/asr_max_memory_smoke", Some(BenchmarkType::AsrMaxMemory))]
    #[case("bogus_id", None)]
    fn from_id_maps_every_benchmark_family(
        #[case] id: &str,
        #[case] expected: Option<BenchmarkType>,
    ) {
        assert_eq!(BenchmarkType::from_id(id), expected);
    }

    /// The ASR timing families' ids never read as asr_eval, whichever family
    /// they map to once those land (mgmt #527), and an asr_eval id is never
    /// theirs.
    #[rstest]
    #[case("asr_throughput_2500ms_128")]
    #[case("asr_max_memory_2500ms")]
    #[case("remote/asr_throughput_2500ms_128")]
    fn an_asr_timing_id_is_not_asr_eval(#[case] id: &str) {
        assert_ne!(BenchmarkType::from_id(id), Some(BenchmarkType::AsrEval));
    }

    #[rstest]
    #[case::local("local", Some(BenchmarkSource::Local))]
    #[case::remote("remote", Some(BenchmarkSource::Remote))]
    #[case::variant_name("Remote", None)]
    fn benchmark_source_wire_spelling(
        #[case] wire: &str,
        #[case] source: Option<BenchmarkSource>,
    ) -> anyhow::Result<()> {
        assert_eq!(
            serde_json::from_value::<BenchmarkSource>(wire.into()).ok(),
            source
        );
        if let Some(source) = source {
            assert_eq!(serde_json::to_value(source)?, wire);
        }
        Ok(())
    }

    /// Every id is distinct, so an accessor reading another kind's id fails here.
    #[test]
    fn each_kind_names_its_id_and_type_and_opens_only_its_own_accessor() -> anyhow::Result<()> {
        let definitions = [
            serde_json::json!({"benchmark_id": "prefill_throughput_512", "benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 512}),
            serde_json::json!({"benchmark_id": "decode_throughput_512_128", "benchmark_type": "decode_throughput", "parameter_prefill_tokens": 512, "parameter_decode_tokens": 128}),
            serde_json::json!({"benchmark_id": "end_to_end_latency_512_128", "benchmark_type": "end_to_end_latency", "parameter_prefill_tokens": 512, "parameter_decode_tokens": 128}),
            serde_json::json!({"benchmark_id": "max_memory_usage_512", "benchmark_type": "max_memory_usage", "parameter_prefill_tokens": 512}),
            serde_json::json!({"benchmark_id": "eval_mmlu_pro", "benchmark_type": "eval", "parameter_eval_id": "mmlu_pro", "parameter_dataset_name": "d", "parameter_max_tokens": 256}),
            serde_json::json!({"benchmark_id": "vl_throughput_224x224", "benchmark_type": "vl_throughput", "parameter_image_width": 224, "parameter_image_height": 224, "parameter_text_tokens": 8, "parameter_decode_tokens": 64}),
            serde_json::json!({"benchmark_id": "vl_max_memory_224x224", "benchmark_type": "vl_max_memory", "parameter_image_width": 224, "parameter_image_height": 224}),
            serde_json::json!({"benchmark_id": "vl_spec_decode_k8", "benchmark_type": "vl_spec_decode", "parameter_draft_length": 8, "parameter_max_tokens": 2048, "dataset": mmspec_corpus()}),
            serde_json::json!({"benchmark_id": "vlm_eval_mmstar", "benchmark_type": "vlm_eval", "parameter_lmms_task": "mmstar", "parameter_dataset_version": "2026.10.1", "parameter_sample_count": 1500, "parameter_max_tokens": 64}),
            asr_eval_wire(),
            serde_json::json!({"benchmark_id": "asr_max_memory_2500ms_text0_decode256", "benchmark_type": "asr_max_memory", "parameter_audio_duration_ms": 2500, "parameter_audio_sample_rate_hz": 16000, "parameter_audio_channels": 1, "parameter_text_tokens": 0, "parameter_decode_tokens": 256}),
            serde_json::json!({"benchmark_id": "asr_throughput_2500ms_text1024_decode256", "benchmark_type": "asr_throughput", "parameter_audio_duration_ms": 2500, "parameter_audio_sample_rate_hz": 16000, "parameter_audio_channels": 1, "parameter_text_tokens": 1024, "parameter_decode_tokens": 256}),
        ];
        // One definition per type, so a new type fails here until it is added.
        let types: std::collections::HashSet<String> = definitions
            .iter()
            .map(|wire| wire["benchmark_type"].to_string())
            .collect();
        assert_eq!(types.len(), BenchmarkType::ALL.len(), "{types:?}");
        definitions
            .into_iter()
            .enumerate()
            .try_for_each(|(kind, wire)| {
                let d: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
                assert_eq!(d.benchmark_id().as_str(), wire["benchmark_id"]);
                assert_eq!(d.benchmark_type().to_string(), wire["benchmark_type"]);
                let opened = [
                    d.as_prefill_throughput().is_ok(),
                    d.as_decode_throughput().is_ok(),
                    d.as_end_to_end_latency().is_ok(),
                    d.as_max_memory_usage().is_ok(),
                    d.as_eval().is_ok(),
                    d.as_vl_throughput().is_ok(),
                    d.as_vl_max_memory().is_ok(),
                    d.as_vl_spec_decode().is_ok(),
                    d.as_vlm_eval().is_ok(),
                    d.as_asr_eval().is_ok(),
                    d.as_asr_max_memory().is_ok(),
                    d.as_asr_throughput().is_ok(),
                ];
                let expected: Vec<bool> = (0..opened.len()).map(|i| i == kind).collect();
                anyhow::ensure!(opened.as_slice() == expected, "{d:?}: {opened:?}");
                Ok(())
            })
    }

    #[test]
    fn a_batch_size_is_its_in_flight_count() -> anyhow::Result<()> {
        assert_eq!(BatchSize::try_new(8)?.in_flight()?.get(), 8);
        Ok(())
    }

    #[test]
    fn as_eval_errors_on_prefill_definition() -> anyhow::Result<()> {
        let def = BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_100".to_owned())?,
            parameter_prefill_tokens: NonNegativeCount::try_new(100)?,
            parameter_batch_size: BatchSize::one(),
        });
        match def.as_eval() {
            Err(BenchmarkDefinitionError {
                expected,
                got_id,
                got_type,
            }) => {
                assert_eq!(expected, "eval");
                assert_eq!(got_id, "prefill_throughput_100");
                assert_eq!(got_type, "prefill_throughput");
            }
            other => anyhow::bail!("expected BenchmarkDefinitionError, got {other:?}"),
        }
        Ok(())
    }

    /// A `vl_spec_decode` document, corpus pin included, survives the catalog
    /// round trip.
    #[test]
    fn vl_spec_decode_definition_round_trips() -> anyhow::Result<()> {
        let wire = serde_json::json!({
            "benchmark_id": "vl_spec_decode_mmspec_test_gqa_k8_bs8",
            "benchmark_type": "vl_spec_decode",
            "parameter_draft_length": 8,
            "parameter_max_tokens": 2048,
            "parameter_batch_size": 8,
            "parameter_temperature": 0.0,
            "dataset": mmspec_corpus(),
        });
        let spec: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
        assert_eq!(spec.benchmark_type(), BenchmarkType::VlSpecDecode);
        assert_eq!(
            spec.as_vl_spec_decode()?
                .parameter_draft_length
                .into_inner(),
            8
        );
        assert_eq!(spec.as_vl_spec_decode()?.parameter_batch_size.as_u32(), 8);
        assert_eq!(
            spec.dataset().and_then(|d| d.subset.as_deref()),
            Some("GQA")
        );
        assert_eq!(serde_json::to_value(&spec)?, wire);
        Ok(())
    }

    /// An omitted batch size means the reference procedure: one sample in flight.
    #[test]
    fn a_vl_spec_decode_definition_without_a_batch_size_runs_one_in_flight() -> anyhow::Result<()> {
        let wire = serde_json::json!({
            "benchmark_id": "vl_spec_decode_mmspec_test_gqa_k8",
            "benchmark_type": "vl_spec_decode",
            "parameter_draft_length": 8,
            "parameter_max_tokens": 2048,
            "dataset": mmspec_corpus(),
        });
        let spec: BenchmarkDefinition = serde_json::from_value(wire)?;
        assert_eq!(
            spec.as_vl_spec_decode()?.parameter_batch_size,
            BatchSize::one()
        );
        assert_eq!(spec.batch_size(), Some(BatchSize::one()));
        Ok(())
    }

    /// The timing kinds read an omitted batch size as `1`, and the batched form
    /// round-trips.
    #[rstest]
    #[case::prefill_reference(
        serde_json::json!({
            "benchmark_id": "prefill_throughput_512",
            "benchmark_type": "prefill_throughput",
            "parameter_prefill_tokens": 512,
        }),
        1
    )]
    #[case::prefill_batched(
        serde_json::json!({
            "benchmark_id": "prefill_throughput_512_bs4",
            "benchmark_type": "prefill_throughput",
            "parameter_prefill_tokens": 512,
            "parameter_batch_size": 4,
        }),
        4
    )]
    #[case::decode_reference(
        serde_json::json!({
            "benchmark_id": "decode_throughput_512_256",
            "benchmark_type": "decode_throughput",
            "parameter_prefill_tokens": 512,
            "parameter_decode_tokens": 256,
        }),
        1
    )]
    #[case::decode_batched(
        serde_json::json!({
            "benchmark_id": "decode_throughput_512_256_bs8",
            "benchmark_type": "decode_throughput",
            "parameter_prefill_tokens": 512,
            "parameter_decode_tokens": 256,
            "parameter_batch_size": 8,
        }),
        8
    )]
    #[case::e2e_reference(
        serde_json::json!({
            "benchmark_id": "end_to_end_latency_512_256",
            "benchmark_type": "end_to_end_latency",
            "parameter_prefill_tokens": 512,
            "parameter_decode_tokens": 256,
        }),
        1
    )]
    #[case::e2e_batched(
        serde_json::json!({
            "benchmark_id": "end_to_end_latency_512_256_bs4",
            "benchmark_type": "end_to_end_latency",
            "parameter_prefill_tokens": 512,
            "parameter_decode_tokens": 256,
            "parameter_batch_size": 4,
        }),
        4
    )]
    fn a_timing_definition_reads_its_batch_size_and_defaults_it_to_one(
        #[case] wire: Value,
        #[case] expected: u32,
    ) -> anyhow::Result<()> {
        let spec: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
        assert_eq!(spec.batch_size().map(BatchSize::as_u32), Some(expected));
        // The wire form always carries the field once parsed, so a
        // re-serialized reference entry names its concurrency explicitly.
        let mut expected_wire = wire;
        expected_wire["parameter_batch_size"] = Value::from(expected);
        assert_eq!(serde_json::to_value(&spec)?, expected_wire);
        Ok(())
    }

    #[test]
    fn a_kind_without_a_batch_size_has_none() -> anyhow::Result<()> {
        let spec = BenchmarkDefinition::MaxMemoryUsage(MaxMemoryUsageBenchmark {
            benchmark_id: BenchmarkId::try_new("max_memory_usage_512".to_owned())?,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
        });
        assert_eq!(spec.batch_size(), None);
        Ok(())
    }

    /// Zero fails at the wire; see [`BatchSize`].
    #[test]
    fn a_zero_batch_size_is_refused_at_the_wire() {
        let wire = serde_json::json!({
            "benchmark_id": "end_to_end_latency_512_256_bs0",
            "benchmark_type": "end_to_end_latency",
            "parameter_prefill_tokens": 512,
            "parameter_decode_tokens": 256,
            "parameter_batch_size": 0,
        });
        assert!(serde_json::from_value::<BenchmarkDefinition>(wire).is_err());
        assert!(BatchSize::try_new(0).is_err());
    }

    /// The warehouse column is Int32, so a batch past it is refused at the wire.
    #[rstest]
    #[case::max("2147483647", true)]
    #[case::past_max("2147483648", false)]
    fn a_batch_size_fits_its_warehouse_column(#[case] json: &str, #[case] ok: bool) {
        assert_eq!(
            serde_json::from_str::<BatchSize>(json).is_ok(),
            ok,
            "{json}"
        );
    }

    #[test]
    fn a_benchmark_that_pulls_no_corpus_has_no_dataset() -> anyhow::Result<()> {
        let spec = BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_512".to_owned())?,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
            parameter_batch_size: BatchSize::one(),
        });
        assert!(spec.dataset().is_none());
        Ok(())
    }

    #[rstest::rstest]
    #[case::prefill("prefill_throughput_512", |benchmark_id| Ok(BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
        benchmark_id,
        parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
        parameter_batch_size: BatchSize::one(),
    })))]
    #[case::eval("eval_mmlu_pro_edge_2026.03.1", |benchmark_id| Ok(BenchmarkDefinition::Eval(EvalBenchmark {
        benchmark_id,
        parameter_eval_id: "mmlu_pro".into(),
        parameter_dataset_name: "edge_2026.03.1".to_string(),
        parameter_max_tokens: PositiveCount::try_new(256)?,
        parameter_max_context: None,
        parameter_mcq_choices: Some(vec!["A".to_string(), "B".to_string()]),
        parameter_temperature: None,
        parameter_top_p: None,
        parameter_top_k: None,
        parameter_min_p: None,
        parameter_seed: None,
        samples: Some(vec![Value::from("sample")]),
    })))]
    #[case::vl_throughput("vl_throughput_224x224_8_64", |benchmark_id| Ok(BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
        benchmark_id,
        parameter_image_width: PositiveCount::try_new(224)?,
        parameter_image_height: PositiveCount::try_new(224)?,
        parameter_text_tokens: NonNegativeCount::try_new(8)?,
        parameter_decode_tokens: PositiveCount::try_new(64)?,
        parameter_num_images: NonNegativeCount::try_new(1)?,
    })))]
    #[case::vl_max_memory("vl_max_memory_224x224_t0_f1", |benchmark_id| Ok(BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
        benchmark_id,
        parameter_image_width: PositiveCount::try_new(224)?,
        parameter_image_height: PositiveCount::try_new(224)?,
        parameter_text_tokens: NonNegativeCount::try_new(0)?,
        parameter_decode_tokens: None,
        parameter_num_images: NonNegativeCount::try_new(1)?,
    })))]
    fn benchmark_definition_round_trips(
        #[case] id: &str,
        #[case] build: fn(BenchmarkId) -> anyhow::Result<BenchmarkDefinition>,
    ) -> anyhow::Result<()> {
        let spec = build(BenchmarkId::try_new(id.to_owned())?)?;
        assert_eq!(
            serde_json::from_value::<BenchmarkDefinition>(serde_json::to_value(&spec)?)?,
            spec
        );
        Ok(())
    }

    #[test]
    fn benchmark_definition_serializes_to_flat_document() -> anyhow::Result<()> {
        let spec = BenchmarkDefinition::Eval(EvalBenchmark {
            benchmark_id: BenchmarkId::try_new("eval_mmlu_pro_edge_2026.03.1".to_owned())?,
            parameter_eval_id: "mmlu_pro".into(),
            parameter_dataset_name: "edge_2026.03.1".to_string(),
            parameter_max_tokens: PositiveCount::try_new(256)?,
            parameter_max_context: None,
            parameter_mcq_choices: Some(vec!["A".to_string(), "B".to_string()]),
            parameter_temperature: None,
            parameter_top_p: None,
            parameter_top_k: None,
            parameter_min_p: None,
            parameter_seed: None,
            samples: Some(vec![Value::from("sample")]),
        });

        let value = serde_json::to_value(&spec)?;
        assert_eq!(
            value,
            serde_json::json!({
                "benchmark_id": "eval_mmlu_pro_edge_2026.03.1",
                "benchmark_type": "eval",
                "parameter_eval_id": "mmlu_pro",
                "parameter_dataset_name": "edge_2026.03.1",
                "parameter_max_tokens": 256,
                "parameter_mcq_choices": ["A", "B"],
                "samples": ["sample"],
            })
        );
        Ok(())
    }

    #[test]
    fn eval_temperature_non_eval_is_greedy() -> anyhow::Result<()> {
        let prefill = BenchmarkDefinition::PrefillThroughput(PrefillThroughputBenchmark {
            benchmark_id: BenchmarkId::try_new("prefill_throughput_512".to_owned())?,
            parameter_prefill_tokens: NonNegativeCount::try_new(512)?,
            parameter_batch_size: BatchSize::one(),
        });
        assert_eq!(prefill.eval_temperature()?, SamplingTemperature::greedy());
        Ok(())
    }

    #[test]
    fn a_vl_entry_without_an_image_count_sends_one() -> anyhow::Result<()> {
        let throughput: BenchmarkDefinition = serde_json::from_value(serde_json::json!({
            "benchmark_id": "vl_throughput_224x224_8_64",
            "benchmark_type": "vl_throughput",
            "parameter_image_width": 224,
            "parameter_image_height": 224,
            "parameter_text_tokens": 8,
            "parameter_decode_tokens": 64,
        }))?;
        assert_eq!(
            throughput
                .as_vl_throughput()?
                .parameter_num_images
                .into_inner(),
            1
        );
        let max_memory: BenchmarkDefinition = serde_json::from_value(serde_json::json!({
            "benchmark_id": "vl_max_memory_224x224",
            "benchmark_type": "vl_max_memory",
            "parameter_image_width": 224,
            "parameter_image_height": 224,
        }))?;
        assert_eq!(
            max_memory
                .as_vl_max_memory()?
                .parameter_num_images
                .into_inner(),
            1
        );
        Ok(())
    }

    fn eval_wire() -> serde_json::Value {
        serde_json::json!({
            "benchmark_id": "eval_math_500_2026.06.1",
            "benchmark_type": "eval",
            "parameter_eval_id": "math_500",
            "parameter_dataset_name": "2026.06.1",
            "parameter_max_tokens": 8192,
        })
    }

    /// An entry that pins nothing parses as pinning nothing: the client's policy
    /// decides.
    #[test]
    fn eval_sampling_is_absent_until_pinned() -> anyhow::Result<()> {
        let bare: BenchmarkDefinition = serde_json::from_value(eval_wire())?;
        let bare = bare.as_eval()?;
        assert_eq!(
            (
                bare.parameter_temperature,
                bare.parameter_top_p,
                bare.parameter_top_k,
                bare.parameter_min_p,
                bare.parameter_seed,
            ),
            (None, None, None, None, None)
        );

        let mut wire = eval_wire();
        wire["parameter_temperature"] = serde_json::json!(1.0);
        wire["parameter_top_p"] = serde_json::json!(0.95);
        wire["parameter_top_k"] = serde_json::json!(64);
        wire["parameter_min_p"] = serde_json::json!(0.05);
        wire["parameter_seed"] = serde_json::json!(7);
        let pinned: BenchmarkDefinition = serde_json::from_value(wire)?;
        let pinned = pinned.as_eval()?;
        assert_eq!(
            (
                pinned.parameter_temperature,
                pinned.parameter_top_p,
                pinned.parameter_top_k,
                pinned.parameter_min_p,
                pinned.parameter_seed,
            ),
            (
                Some(SamplingTemperature::try_new(1.0)?),
                Some(TopP::try_new(0.95)?),
                Some(TopK::try_new(64)?),
                Some(MinP::try_new(0.05)?),
                Some(SamplingSeed::try_new(7)?),
            )
        );
        Ok(())
    }

    /// A knob outside its range is refused where the document is read, not
    /// clamped on the device. The bounds themselves are pinned in `sampling`.
    #[rstest]
    #[case::temperature_high("parameter_temperature", serde_json::json!(2.1))]
    #[case::top_p_high("parameter_top_p", serde_json::json!(1.5))]
    #[case::min_p_high("parameter_min_p", serde_json::json!(1.5))]
    #[case::top_k_negative("parameter_top_k", serde_json::json!(-1))]
    #[case::seed_past_u32("parameter_seed", serde_json::json!(4_294_967_296i64))]
    fn eval_sampling_out_of_range_is_refused(#[case] key: &str, #[case] value: serde_json::Value) {
        let mut wire = eval_wire();
        wire[key] = value.clone();
        assert!(
            serde_json::from_value::<BenchmarkDefinition>(wire).is_err(),
            "{key}={value} must be refused"
        );
    }

    /// An edge is a pin like any other: greedy, an open nucleus and a disabled
    /// cut have to land as values, or no plan can ask for them.
    #[rstest]
    #[case::greedy("parameter_temperature", serde_json::json!(0.0))]
    #[case::hottest("parameter_temperature", serde_json::json!(2.0))]
    #[case::top_p_open("parameter_top_p", serde_json::json!(1.0))]
    #[case::min_p_off("parameter_min_p", serde_json::json!(0.0))]
    #[case::top_k_off("parameter_top_k", serde_json::json!(0))]
    #[case::seed_zero("parameter_seed", serde_json::json!(0))]
    fn eval_sampling_edges_land_as_values(
        #[case] key: &str,
        #[case] value: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut wire = eval_wire();
        wire[key] = value.clone();
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        assert_eq!(
            serde_json::to_value(&parsed)?[key],
            value,
            "{key} must survive as a value"
        );
        Ok(())
    }

    /// The catalog decides when it says so; the eval-id policy decides when it
    /// does not. A pinned `0.0` is greedy, not an absence.
    #[rstest]
    #[case::pinned_greedy(Some(0.0), 0.0)]
    #[case::pinned_hot(Some(1.0), 1.0)]
    #[case::unpinned(None, 0.6)]
    fn a_pinned_temperature_wins_over_the_eval_id_policy(
        #[case] pinned: Option<f64>,
        #[case] expected: f64,
    ) -> anyhow::Result<()> {
        let mut wire = eval_wire();
        if let Some(pinned) = pinned {
            wire["parameter_temperature"] = serde_json::json!(pinned);
        }
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        assert_eq!(parsed.eval_temperature()?.as_f64(), expected);
        Ok(())
    }

    /// The repeats of one sample must be independent draws, so each `#k` gets
    /// its own seed; a suffix that is not a repeat index leaves the base alone.
    #[rstest]
    #[case::first("row-3#0", 7)]
    #[case::third("row-3#2", 9)]
    #[case::no_suffix("row-3", 7)]
    #[case::not_an_index("row-3#draft", 7)]
    #[case::negative_suffix("row-3#-5", 7)]
    fn a_samples_seed_offsets_by_its_repeat_index(
        #[case] sample_id: &str,
        #[case] expected: i64,
    ) -> anyhow::Result<()> {
        let mut wire = eval_wire();
        wire["parameter_seed"] = serde_json::json!(7);
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        assert_eq!(parsed.as_eval()?.sample_seed(sample_id), Some(expected));
        Ok(())
    }

    fn asr_eval_wire() -> serde_json::Value {
        serde_json::json!({
            "benchmark_id": "asr_eval_fleurs_nl_nl_2026.10.1_voxtral",
            "benchmark_type": "asr_eval",
            "parameter_asr_task": "asr_fleurs_nl_nl",
            "parameter_dataset_version": "2026.10.1",
            "parameter_sample_count": 364,
            "parameter_prompt_profile": "voxtral",
            "parameter_max_tokens": 500,
            "parameter_temperature": 0.0
        })
    }

    /// An asr_eval entry round-trips, reads its task and profile typed, and
    /// names its error unit from the task.
    #[test]
    fn an_asr_eval_entry_round_trips_typed() -> anyhow::Result<()> {
        let parsed: BenchmarkDefinition = serde_json::from_value(asr_eval_wire())?;
        assert_eq!(serde_json::to_value(&parsed)?, asr_eval_wire());
        assert_eq!(parsed.benchmark_type(), BenchmarkType::AsrEval);
        let asr = parsed.as_asr_eval()?;
        assert_eq!(
            asr.parameter_asr_task,
            AsrEvalTask::Known(KnownAsrEvalTask::AsrFleursNlNl)
        );
        assert_eq!(
            asr.parameter_prompt_profile,
            AsrPromptProfile::Known(KnownAsrPromptProfile::Voxtral)
        );
        assert_eq!(
            asr.parameter_asr_task.error_unit(),
            Some(AsrErrorUnit::Word)
        );
        assert!(!asr.beam_search());
        asr.check_decoding()?;
        Ok(())
    }

    /// The prompt profile is required: the evals server has no default ASR
    /// recipe, and every model family is asked its own way.
    #[test]
    fn an_asr_eval_entry_without_a_profile_is_refused() -> anyhow::Result<()> {
        let mut wire = asr_eval_wire();
        wire.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("an object"))?
            .remove("parameter_prompt_profile");
        assert!(serde_json::from_value::<BenchmarkDefinition>(wire).is_err());
        Ok(())
    }

    /// MiniCPM-o's beam search parses; its penalties need beams, and sampling
    /// knobs cannot ride beside them. Each profile's own rules are
    /// `asr_decoding`'s tests.
    #[rstest]
    #[case::beams(serde_json::json!({"parameter_num_beams": 3, "parameter_beam_repetition_penalty": 1.2, "parameter_beam_length_penalty": 1.0}), Ok(true))]
    #[case::greedy(serde_json::json!({}), Ok(false))]
    #[case::one_beam(serde_json::json!({"parameter_num_beams": 1}), Err(AsrDecodingError::SingleBeam))]
    #[case::penalty_without_beams(serde_json::json!({"parameter_beam_repetition_penalty": 1.2}), Err(AsrDecodingError::BeamPenaltyWithoutBeams { knob: AsrDecodingKnob::BeamRepetitionPenalty }))]
    #[case::sampling_with_beams(serde_json::json!({"parameter_num_beams": 3, "parameter_temperature": 0.7}), Err(AsrDecodingError::SamplingWithBeams { knob: AsrDecodingKnob::Temperature }))]
    fn asr_eval_decoding_is_coherent(
        #[case] knobs: serde_json::Value,
        #[case] expected: Result<bool, AsrDecodingError>,
    ) -> anyhow::Result<()> {
        let mut wire = asr_eval_wire();
        let fields = wire
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("an object"))?;
        fields.remove("parameter_temperature");
        fields.insert("parameter_prompt_profile".into(), "minicpm_o".into());
        fields.extend(
            knobs
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("an object"))?
                .clone(),
        );
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        let asr = parsed.as_asr_eval()?;
        assert_eq!(asr.check_decoding().map(|()| asr.beam_search()), expected);
        Ok(())
    }

    #[rstest]
    #[case::zero_beams(serde_json::json!({"parameter_num_beams": 0}))]
    #[case::too_many_beams(serde_json::json!({"parameter_num_beams": 9}))]
    #[case::zero_penalty(serde_json::json!({"parameter_num_beams": 3, "parameter_beam_repetition_penalty": 0.0}))]
    fn asr_eval_beam_knobs_out_of_range_are_refused(
        #[case] knobs: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut wire = asr_eval_wire();
        wire.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("an object"))?
            .extend(
                knobs
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("an object"))?
                    .clone(),
            );
        assert!(serde_json::from_value::<BenchmarkDefinition>(wire).is_err());
        Ok(())
    }

    /// The derived seed wraps inside the narrowest runtime's `uint32_t`.
    #[test]
    fn a_derived_seed_wraps_inside_the_cap() -> anyhow::Result<()> {
        let mut wire = eval_wire();
        wire["parameter_seed"] = serde_json::json!(4_294_967_295i64);
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        let eval = parsed.as_eval()?;
        assert_eq!(eval.sample_seed("row-1#0"), Some(4_294_967_295));
        assert_eq!(eval.sample_seed("row-1#1"), Some(0));
        assert_eq!(eval.sample_seed("row-1#2"), Some(1));

        let unseeded: BenchmarkDefinition = serde_json::from_value(eval_wire())?;
        assert_eq!(unseeded.as_eval()?.sample_seed("row-1#1"), None);
        Ok(())
    }

    /// An entry that pins nothing sends nothing: a `deny_unknown_fields` reader
    /// downstream would refuse five null keys.
    #[test]
    fn unpinned_sampling_is_elided_from_the_wire() -> anyhow::Result<()> {
        let parsed: BenchmarkDefinition = serde_json::from_value(eval_wire())?;
        let value = serde_json::to_value(&parsed)?;
        [
            "parameter_temperature",
            "parameter_top_p",
            "parameter_top_k",
            "parameter_min_p",
            "parameter_seed",
        ]
        .iter()
        .for_each(|key| assert!(value.get(key).is_none(), "{key} must be absent: {value}"));
        Ok(())
    }

    /// Served byte for byte as before, so a `deny_unknown_fields` reader is unaffected.
    #[test]
    fn an_unbounded_entry_is_unchanged_on_the_wire() -> anyhow::Result<()> {
        let parsed: BenchmarkDefinition = serde_json::from_value(eval_wire())?;
        assert_eq!(parsed.as_eval()?.parameter_max_context, None);
        assert_eq!(
            serde_json::to_string(&parsed)?,
            r#"{"benchmark_type":"eval","benchmark_id":"eval_math_500_2026.06.1","parameter_eval_id":"math_500","parameter_dataset_name":"2026.06.1","parameter_max_tokens":8192}"#
        );
        Ok(())
    }

    #[test]
    fn a_context_bound_round_trips() -> anyhow::Result<()> {
        let mut wire = eval_wire();
        wire["parameter_max_tokens"] = serde_json::json!(32768);
        wire["parameter_max_context"] = serde_json::json!(32768);
        let parsed: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
        assert_eq!(
            parsed.as_eval()?.parameter_max_context,
            Some(PositiveCount::try_new(32768)?)
        );
        assert_eq!(serde_json::to_value(&parsed)?, wire);
        Ok(())
    }

    #[rstest]
    #[case::zero(serde_json::json!(0))]
    #[case::negative(serde_json::json!(-1))]
    #[case::past_i32(serde_json::json!(2_147_483_648i64))]
    fn a_context_bound_out_of_range_is_refused(#[case] value: serde_json::Value) {
        let mut wire = eval_wire();
        wire["parameter_max_context"] = value.clone();
        assert!(
            serde_json::from_value::<BenchmarkDefinition>(wire).is_err(),
            "{value} must be refused"
        );
    }

    #[rstest]
    #[case::unbounded(None, false, Ok(()))]
    #[case::at_the_cap(Some(8192), false, Ok(()))]
    #[case::under_the_cap(Some(4096), false, Ok(()))]
    #[case::past_the_cap(
        Some(8193),
        false,
        Err("parameter_max_context (8193) exceeds parameter_max_tokens (8192)")
    )]
    #[case::mcq(Some(4096), true, Err("beside parameter_mcq_choices"))]
    #[case::mcq_unbounded(None, true, Ok(()))]
    fn a_context_bound_binds_and_is_never_mcq(
        #[case] max_context: Option<i32>,
        #[case] mcq: bool,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let mut wire = eval_wire();
        if let Some(max_context) = max_context {
            wire["parameter_max_context"] = serde_json::json!(max_context);
        }
        if mcq {
            wire["parameter_mcq_choices"] = serde_json::json!(["A", "B"]);
        }
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        let checked = parsed
            .as_eval()?
            .check_max_context()
            .map_err(|e| e.to_string());
        match expected {
            Ok(()) => assert_eq!(checked, Ok(())),
            Err(names) => assert!(
                checked.as_ref().is_err_and(|e| e.contains(names)),
                "{checked:?}"
            ),
        }
        Ok(())
    }

    fn vlm_eval_wire() -> serde_json::Value {
        serde_json::json!({
            "benchmark_id": "vlm_eval_mmstar",
            "benchmark_type": "vlm_eval",
            "parameter_lmms_task": "mmstar",
            "parameter_dataset_version": "2026.10.1",
            "parameter_sample_count": 1500,
            "parameter_max_tokens": 8192,
        })
    }

    /// An entry that pins nothing is greedy and served as before;
    /// a pinned knob lands as itself.
    #[test]
    fn vlm_eval_sampling_is_greedy_until_pinned() -> anyhow::Result<()> {
        let bare: BenchmarkDefinition = serde_json::from_value(vlm_eval_wire())?;
        assert_eq!(serde_json::to_value(&bare)?, vlm_eval_wire());
        assert_eq!(bare.eval_temperature()?, SamplingTemperature::greedy());
        assert_eq!(bare.as_vlm_eval()?.sample_seed("row-1#1"), None);

        let mut wire = vlm_eval_wire();
        wire["parameter_temperature"] = serde_json::json!(0.7);
        wire["parameter_top_p"] = serde_json::json!(0.8);
        wire["parameter_top_k"] = serde_json::json!(20);
        wire["parameter_min_p"] = serde_json::json!(0.0);
        wire["parameter_presence_penalty"] = serde_json::json!(1.5);
        wire["parameter_repeat_penalty"] = serde_json::json!(1.05);
        wire["parameter_seed"] = serde_json::json!(7);
        let pinned: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
        let vlm = pinned.as_vlm_eval()?;
        assert_eq!(
            (
                vlm.sampling_temperature(),
                vlm.parameter_top_p,
                vlm.parameter_top_k,
                vlm.parameter_min_p,
                vlm.parameter_presence_penalty,
                vlm.parameter_repeat_penalty,
                vlm.parameter_seed,
            ),
            (
                SamplingTemperature::try_new(0.7)?,
                Some(TopP::try_new(0.8)?),
                Some(TopK::try_new(20)?),
                Some(MinP::try_new(0.0)?),
                Some(PresencePenalty::try_new(1.5)?),
                Some(RepeatPenalty::try_new(1.05)?),
                Some(SamplingSeed::try_new(7)?),
            )
        );
        assert_eq!(serde_json::to_value(&pinned)?, wire);
        Ok(())
    }

    /// The task and a vision sample read typed and serve back as sent.
    #[test]
    fn a_vlm_eval_entry_reads_its_task_and_samples_typed() -> anyhow::Result<()> {
        let mut wire = vlm_eval_wire();
        wire["samples"] = serde_json::json!([{"id": "s1", "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}},
            {"type": "text", "text": "Which shape is red?"},
        ]}]}]);
        let parsed: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
        let vlm = parsed.as_vlm_eval()?;
        assert_eq!(
            vlm.parameter_lmms_task,
            VlmEvalTask::Known(KnownVlmEvalTask::Mmstar)
        );
        assert_eq!(vlm.samples.as_ref().map(Vec::len), Some(1));
        assert_eq!(serde_json::to_value(&parsed)?, wire);
        Ok(())
    }

    /// An entry without a prompt profile serves as before; a named one, known
    /// or not, reads typed and serves back as written.
    #[rstest]
    #[case::dataset_prompt(None, None)]
    #[case::known(
        Some("minicpm_v_cot"),
        Some(VlmPromptProfile::Known(KnownVlmPromptProfile::MinicpmVCot))
    )]
    #[case::unknown(
        Some("qwen2_vl"),
        Some(VlmPromptProfile::Unknown("qwen2_vl".to_string()))
    )]
    fn a_vlm_eval_entry_reads_its_prompt_profile_typed(
        #[case] named: Option<&str>,
        #[case] expected: Option<VlmPromptProfile>,
    ) -> anyhow::Result<()> {
        let mut wire = vlm_eval_wire();
        if let Some(named) = named {
            wire["parameter_prompt_profile"] = named.into();
        }
        let parsed: BenchmarkDefinition = serde_json::from_value(wire.clone())?;
        assert_eq!(parsed.as_vlm_eval()?.parameter_prompt_profile, expected);
        assert_eq!(serde_json::to_value(&parsed)?, wire);
        Ok(())
    }

    #[rstest]
    #[case::temperature_high("parameter_temperature", serde_json::json!(2.1))]
    #[case::top_p_high("parameter_top_p", serde_json::json!(1.5))]
    #[case::min_p_high("parameter_min_p", serde_json::json!(1.5))]
    #[case::top_k_negative("parameter_top_k", serde_json::json!(-1))]
    #[case::presence_penalty_low("parameter_presence_penalty", serde_json::json!(-2.5))]
    #[case::repeat_penalty_zero("parameter_repeat_penalty", serde_json::json!(0.0))]
    #[case::seed_past_u32("parameter_seed", serde_json::json!(4_294_967_296i64))]
    fn vlm_eval_sampling_out_of_range_is_refused(
        #[case] key: &str,
        #[case] value: serde_json::Value,
    ) {
        let mut wire = vlm_eval_wire();
        wire[key] = value.clone();
        assert!(
            serde_json::from_value::<BenchmarkDefinition>(wire).is_err(),
            "{key}={value} must be refused"
        );
    }

    /// A vlm_eval or asr_eval entry on [`SeedRule::PerSampleSha256`] sends each
    /// sample the first four bytes of `sha256("<seed>|<sample_id>")` in UTF-8,
    /// big-endian. The expected values come from Python's `hashlib`, so they
    /// pin the rule, not this implementation.
    #[rstest]
    #[case::bare_id(0, "row-3", 1_612_673_859)]
    #[case::repeat_0(0, "row-3#0", 938_398_776)]
    #[case::repeat_1(0, "row-3#1", 1_708_530_419)]
    #[case::other_seed(7, "row-3#0", 892_101_227)]
    #[case::other_repeat(7, "row-3#2", 3_501_035_444)]
    #[case::largest_seed(4_294_967_295, "row-1#1", 1_422_242_358)]
    #[case::non_ascii_id(0, "élément-3#1", 1_252_465_368)]
    #[case::non_latin_id(7, "图像-2", 225_674_150)]
    fn a_per_sample_entry_seeds_each_sample_by_hash(
        #[case] seed: i64,
        #[case] sample_id: &str,
        #[case] expected: i64,
    ) -> anyhow::Result<()> {
        let mut vlm = vlm_eval_wire();
        vlm["parameter_seed"] = serde_json::json!(seed);
        vlm["parameter_seed_rule"] = serde_json::json!("per_sample_sha256");
        let vlm: BenchmarkDefinition = serde_json::from_value(vlm)?;
        assert_eq!(vlm.as_vlm_eval()?.sample_seed(sample_id), Some(expected));
        let mut asr = asr_eval_wire();
        asr["parameter_seed"] = serde_json::json!(seed);
        asr["parameter_seed_rule"] = serde_json::json!("per_sample_sha256");
        let asr: BenchmarkDefinition = serde_json::from_value(asr)?;
        let BenchmarkDefinition::AsrEval(asr) = asr else {
            anyhow::bail!("an asr_eval entry");
        };
        assert_eq!(asr.sample_seed(sample_id), Some(expected));
        Ok(())
    }

    /// An entry that names no rule draws exactly as before: the base seed offset
    /// by the repeat index, the text eval's rule.
    #[rstest]
    #[case::first("row-3#0", 7)]
    #[case::third("row-3#2", 9)]
    #[case::no_suffix("row-3", 7)]
    fn an_entry_without_a_seed_rule_keeps_the_repeat_offset(
        #[case] sample_id: &str,
        #[case] expected: i64,
    ) -> anyhow::Result<()> {
        let mut vlm = vlm_eval_wire();
        vlm["parameter_seed"] = serde_json::json!(7);
        let vlm: BenchmarkDefinition = serde_json::from_value(vlm)?;
        assert_eq!(vlm.as_vlm_eval()?.sample_seed(sample_id), Some(expected));
        let mut asr = asr_eval_wire();
        asr["parameter_seed"] = serde_json::json!(7);
        let asr: BenchmarkDefinition = serde_json::from_value(asr)?;
        let BenchmarkDefinition::AsrEval(asr) = asr else {
            anyhow::bail!("an asr_eval entry");
        };
        assert_eq!(asr.sample_seed(sample_id), Some(expected));
        Ok(())
    }

    /// On the per-sample rule every sample of a vlm_eval draws with a seed of
    /// its own, repeats included: one seed for all of them biases a vLLM run.
    #[test]
    fn a_per_sample_vlm_eval_seed_differs_per_sample() -> anyhow::Result<()> {
        let mut vlm = vlm_eval_wire();
        vlm["parameter_seed"] = serde_json::json!(0);
        vlm["parameter_seed_rule"] = serde_json::json!("per_sample_sha256");
        let vlm: BenchmarkDefinition = serde_json::from_value(vlm)?;
        let vlm = vlm.as_vlm_eval()?;
        let seeds: std::collections::BTreeSet<_> = (0..1000)
            .flat_map(|row| [format!("row-{row}"), format!("row-{row}#1")])
            .map(|id| vlm.sample_seed(&id))
            .collect();
        assert_eq!(seeds.len(), 2000);
        assert!(
            seeds
                .iter()
                .all(|seed| seed.is_some_and(|s| (0..=i64::from(u32::MAX)).contains(&s)))
        );
        Ok(())
    }

    /// The text eval keeps its rule: same ids, same base seed, the seeds its
    /// published results were drawn with.
    #[rstest]
    #[case::bare_id("row-3", 0)]
    #[case::repeat_0("row-3#0", 0)]
    #[case::repeat_1("row-3#1", 1)]
    fn the_text_eval_keeps_the_repeat_offset_rule(
        #[case] sample_id: &str,
        #[case] expected: i64,
    ) -> anyhow::Result<()> {
        let mut eval = eval_wire();
        eval["parameter_seed"] = serde_json::json!(0);
        let eval: BenchmarkDefinition = serde_json::from_value(eval)?;
        assert_eq!(eval.as_eval()?.sample_seed(sample_id), Some(expected));
        Ok(())
    }

    /// `vl_spec_decode` reads an absent temperature as greedy, the reference
    /// procedure, not a gap for a client's own policy.
    #[rstest]
    #[case::absent(None, 0.0)]
    #[case::greedy(Some(0.0), 0.0)]
    #[case::sampled(Some(0.8), 0.8)]
    #[case::hottest(Some(2.0), 2.0)]
    fn vl_spec_decode_temperature_defaults_to_greedy(
        #[case] pinned: Option<f64>,
        #[case] expected: f64,
    ) -> anyhow::Result<()> {
        let mut wire = serde_json::json!({
            "benchmark_id": "vl_spec_decode_k7",
            "benchmark_type": "vl_spec_decode",
            "parameter_draft_length": 7,
            "parameter_max_tokens": 2048,
            "dataset": mmspec_corpus(),
        });
        if let Some(pinned) = pinned {
            wire["parameter_temperature"] = serde_json::json!(pinned);
        }
        let parsed: BenchmarkDefinition = serde_json::from_value(wire)?;
        assert_eq!(
            parsed.as_vl_spec_decode()?.parameter_temperature.as_f64(),
            expected
        );
        Ok(())
    }
}
