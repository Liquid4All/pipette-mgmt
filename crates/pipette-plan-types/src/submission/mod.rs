//! What a finished cell produced: [`BenchmarkSubmissionPayload`] as a client
//! builds it, and [`Submission`], the same result as the server reads it from
//! `POST /benchmarks`, with the receipts the route answers.

mod asr;
pub use asr::{AsrAudioTokensSource, AsrClientExtras, AsrClientExtrasError, AsrContextSizing};

mod upload;
pub use upload::{
    BatchItem, BatchSubmitted, FailureInput, JobAccepted, SampleCompletion, Submission,
    SubmissionInput, SuccessInput,
};

use nutype::nutype;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::benchmark::declared_benchmark_params;
use crate::device::DeviceInfo;
use crate::thermal::{DevicePowerState, ThermalTelemetry};
use crate::{BenchmarkParams, BenchmarkType, Sha256};

// The ranges a reported number must fall in, refused at deserialization so an
// out-of-range reading never reaches a stored row. `f32` and `i32` are the
// widths of the warehouse columns these land in.

/// A measured time in milliseconds that a metric divides by, so zero would
/// score an `inf`. `finite` because `serde_json` reads an overflowing literal
/// such as `1e300` into an `f32` as `inf`.
#[nutype(
    validate(finite, greater = 0.0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct PositiveMillis(f32);

/// A time in milliseconds no metric divides by, such as a standard deviation
/// or one phase of a run, so zero is a real reading.
#[nutype(
    validate(finite, greater_or_equal = 0.0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct NonNegativeMillis(f32);

/// A size or peak in bytes. Zero is a real reading, such as a run that touched
/// no swap.
#[nutype(
    validate(greater_or_equal = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        Display,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct ByteCount(i64);

/// A `u64` past `i64::MAX`, which no [`ByteCount`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("byte count must be at most {max}, got {0}", max = i64::MAX)]
pub struct ByteCountRangeError(pub u64);

impl TryFrom<u64> for ByteCount {
    type Error = ByteCountRangeError;

    fn try_from(bytes: u64) -> Result<Self, Self::Error> {
        i64::try_from(bytes)
            .ok()
            .and_then(|bytes| Self::try_new(bytes).ok())
            .ok_or(ByteCountRangeError(bytes))
    }
}

/// Per-sample stop reason, as the snake_case tokens mgmt reads (see
/// `docs/scoring-service.md`).
///
/// mgmt records a client's value as `recorded` however the client derived it:
/// an engine signal and a heuristic (AFM's token count vs the cap) look alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkEvalCompletionStopReason {
    /// Model emitted EOS before either bound: completion tokens `< cap`,
    /// and prompt plus completion under any `parameter_max_context`.
    Eos,
    /// Hit the output-token cap, completion tokens `== cap`
    /// (`n_predict = parameter_max_tokens`), or the entry's
    /// `parameter_max_context`, prompt plus completion `==` it, which leaves
    /// completion tokens `< cap`.
    Truncated,
    /// The engine stopped on runaway repetition (vLLM's `finish_reason =
    /// "repetition"`), with the reason in `stop_detail`. Older builds wrote it
    /// too, so their checkpoints still load on resume.
    DoomLoop,
    /// The sample produced no completion (empty output or runtime crash); the
    /// detail is in `failed_reason`.
    Failure,
    /// Attempted but unclassifiable at generation: the MCQ arm, an unrecognized
    /// or absent stop signal, a resumed pre-feature checkpoint sample. Clients
    /// never emit absent, so an absent `stop_reason` downstream is a client
    /// predating the field.
    #[default]
    Unknown,
}

/// One eval sample's outcome. A crashed sample has `failed` set and an empty
/// `completion`, which the server excludes from scoring; the local checkpoint
/// keeps it so a later run skips the sample instead of crashing again.
///
/// Every non-core field defaults on parse, so pre-feature checkpoints load.
/// `failed_reason` may start with `[<rfc3339-timestamp>] `; consumers must
/// also accept a bare reason.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchmarkEvalCompletion {
    pub id: String,
    pub completion: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,
    /// Stop reason captured at generation. Clients always write it; a missing
    /// value decodes as `unknown` so a pre-feature checkpoint still loads.
    #[serde(default)]
    pub stop_reason: BenchmarkEvalCompletionStopReason,
    /// The raw signal behind `stop_reason`: the crash detail for `failure`, the
    /// runtime signal for `unknown` (e.g. `stop_type=word`). `None` for a clean
    /// `eos` / `truncated`; replaces `failed_reason` once consumers move over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_detail: Option<String>,
    /// Output tokens for this sample, which tell `eos` (< cap) from `truncated`
    /// (== cap, or < cap when `parameter_max_context` stopped the sample). Each runtime counts its own way, so compare across runtimes
    /// only roughly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
}

/// What a run's memory looked like while it ran: observations that qualify a
/// result rather than score it, on every benchmark kind, so no benchmark
/// prefix. Absence means "not observed", never zero.
///
/// The span is the runtime process that served the cell, from its start, so
/// model load and warm-up count; with a process per rep, the largest. The
/// counters are high-water marks not every platform can reset, so no reps-only
/// figure exists. An eval reports none: a resumed or restarted run ends on a
/// process that served only part of the cell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryObservation {
    /// Peak *resident* memory: the kernel's high-water mark for pages in RAM
    /// (`VmHWM` on Android and Linux, `PeakWorkingSetSize` on Windows; macOS
    /// `phys_footprint` also bills compressed pages). torch-oai instead reports
    /// its server cgroup's largest `anon + shmem + kernel` from `memory.stat`.
    /// Swapped-out pages are not counted: [`Self::max_swap_bytes`] beside it
    /// tells a reader reclaim suppressed it.
    #[serde(
        rename = "observation_max_host_bytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_host_bytes: Option<u64>,
    /// Most the kernel held of this run in swap at once. Not additive with
    /// [`Self::max_host_bytes`]: the two peaks need not coincide. Non-zero, it
    /// says roughly how much the resident figure under-states; `Some(0)` is a
    /// sampled reading that makes the peak trustworthy, `None` that nothing
    /// sampled it.
    #[serde(
        rename = "observation_max_swap_bytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_swap_bytes: Option<u64>,
}

impl MemoryObservation {
    /// A peak from a sampler that sees no swap term. Zero, how every sampler
    /// spells "no read landed", is withheld: on the row it would read as a
    /// measurement. Build through this, not the literal, to keep that rule.
    pub fn host_only(host_bytes: u64) -> Self {
        Self {
            max_host_bytes: (host_bytes > 0).then_some(host_bytes),
            max_swap_bytes: None,
        }
    }

    /// A peak with its swap term. A withheld peak withholds the swap too: half
    /// an observation would claim swap was sampled without saying what it held.
    pub fn with_swap(host_bytes: u64, swap_bytes: u64) -> Self {
        match (host_bytes > 0).then_some(host_bytes) {
            Some(host_bytes) => Self {
                max_host_bytes: Some(host_bytes),
                max_swap_bytes: Some(swap_bytes),
            },
            None => Self::default(),
        }
    }

    /// The larger of two observations, term by term: the worst any rep reached,
    /// as the largest peak and swap need not come from the same rep. `None`
    /// never wins, so a rep that missed a sample erases no reading.
    pub fn merge_max(self, other: Self) -> Self {
        fn larger(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (found, None) | (None, found) => found,
            }
        }
        Self {
            max_host_bytes: larger(self.max_host_bytes, other.max_host_bytes),
            max_swap_bytes: larger(self.max_swap_bytes, other.max_swap_bytes),
        }
    }
}

/// The most of each [`RuntimeOutput`] stream the server keeps, in bytes.
pub const MAX_RUNTIME_OUTPUT_BYTES: usize = 1024 * 1024;

/// The most a submission's `client_extras` may hold, in bytes as sent. The
/// server refuses a larger value, since JSON cut short is no longer JSON.
pub const MAX_CLIENT_EXTRAS_BYTES: usize = 16 * 1024;

/// The mark at the start of a text that [`keep_tail`] cut.
pub const ELLIPSIS: &str = "…";

/// Cut `s` to at most `max_bytes` on a char boundary, keeping the end, where a
/// runtime writes its final error; a cut text starts with [`ELLIPSIS`].
pub fn keep_tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let budget = max_bytes.saturating_sub(ELLIPSIS.len());
    let start = s.ceil_char_boundary(s.len().saturating_sub(budget));
    format!("{ELLIPSIS}{}", s.get(start..).unwrap_or_default())
}

/// The most of `device_hardware_readings` or `device_os_throttling_readings`
/// the server keeps, in bytes, cut with [`keep_last_elements`]. Five reps fit
/// on every platform with room to spare.
pub const MAX_DEVICE_READINGS_BYTES: usize = 128 * 1024;

/// The most of `readiness_gate_waits` the server keeps, in bytes, cut the same
/// way. Five reps' waits are about 1 KiB.
pub const MAX_READINESS_GATE_WAITS_BYTES: usize = 8 * 1024;

/// Cut the JSON array in `s` to at most `max_bytes` by dropping elements from
/// its front, so what is left still parses. Errors when `s` is not an array.
pub fn keep_last_elements(s: &str, max_bytes: usize) -> Result<String, serde_json::Error> {
    let elements: Vec<&RawValue> = serde_json::from_str(s)?;
    if s.len() <= max_bytes {
        return Ok(s.to_owned());
    }
    // The length of `[…]` holding the last one, two, … elements.
    let kept = elements
        .iter()
        .rev()
        .scan(2, |len, element| {
            *len += element.get().len() + usize::from(*len > 2);
            Some(*len)
        })
        .take_while(|len| *len <= max_bytes)
        .count();
    let kept: Vec<&str> = elements
        .iter()
        .skip(elements.len() - kept)
        .map(|element| element.get())
        .collect();
    Ok(format!("[{}]", kept.join(",")))
}

/// What the runtime printed while the benchmark ran: the spawned subprocess,
/// or the app itself for an in-process runtime. Absent means not captured,
/// `Some("")` printed nothing; the server keeps the last
/// [`MAX_RUNTIME_OUTPUT_BYTES`] of each, apart from the body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_stderr: Option<String>,
}

impl RuntimeOutput {
    /// Whether neither stream was captured.
    pub fn is_empty(&self) -> bool {
        self.runtime_stdout.is_none() && self.runtime_stderr.is_none()
    }

    /// Each stream cut to its last [`MAX_RUNTIME_OUTPUT_BYTES`].
    #[must_use]
    pub fn capped(mut self) -> Self {
        [&mut self.runtime_stdout, &mut self.runtime_stderr]
            .into_iter()
            .flatten()
            .filter(|stream| stream.len() > MAX_RUNTIME_OUTPUT_BYTES)
            .for_each(|stream| *stream = keep_tail(stream, MAX_RUNTIME_OUTPUT_BYTES));
        self
    }
}

/// Result data for a benchmark run.
///
/// Untagged: serde picks the first variant whose required fields a body holds,
/// so a variant sharing another's field must come before it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum BenchmarkResultData {
    PrefillThroughput {
        prefill_time_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefill_time_ms_stddev: Option<f64>,
    },
    DecodeThroughput {
        decode_time_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decode_time_ms_stddev: Option<f64>,
    },
    VlThroughput {
        prompt_tokens: u32,
        /// Image-only tokens: the image embeddings alone, without the boundary
        /// markers around them (`prompt_tokens` counts those). `None` when the
        /// client cannot count them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_tokens: Option<u32>,
        prompt_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_ms_stddev: Option<f64>,
        predicted_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predicted_ms_stddev: Option<f64>,
        /// The request's span on the client's clock.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_time_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_time_ms_stddev: Option<f64>,
    },
    /// Speech recognition: the times to prefill one audio clip and to decode
    /// after it. Field names carry an `asr_` prefix because the enum is
    /// untagged and [`Self::VlThroughput`] has the same measurements. Ahead of
    /// [`Self::EndToEndLatency`], whose only required field (`total_time_ms`)
    /// an ASR body can carry too.
    AsrThroughput {
        /// The whole prompt: audio, text (the entry's and the ask) and template
        /// tokens. The same on every rep.
        asr_prompt_tokens: u32,
        /// The audio-only part of `asr_prompt_tokens`, as the runtime's
        /// multimodal layer counts it.
        asr_audio_tokens: u32,
        /// Time to process all `asr_prompt_tokens`, the audio encode included.
        asr_prompt_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asr_prompt_ms_stddev: Option<f64>,
        /// Time to generate the decode tokens.
        asr_predicted_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asr_predicted_ms_stddev: Option<f64>,
        /// The request's span on the client's clock, as `vl_throughput` sends
        /// it: `end_to_end_latency`. `None` from a client that does not time it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_time_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_time_ms_stddev: Option<f64>,
    },
    EndToEndLatency {
        total_time_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_time_ms_stddev: Option<f64>,
    },
    MaxMemoryUsage {
        // Sent as `max_ram_bytes` / `max_vram_bytes`, the names every mgmt
        // reads; mgmt also reads the `max_host_bytes` / `max_gpu_bytes` spelling.
        #[serde(rename = "max_ram_bytes", alias = "max_host_bytes")]
        max_host_bytes: u64,
        #[serde(
            rename = "max_vram_bytes",
            alias = "max_gpu_bytes",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        max_gpu_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_npu_bytes: Option<u64>,
    },
    Eval {
        completions: Vec<BenchmarkEvalCompletion>,
    },
    /// One traversal of an image + text corpus (DSpark), with speculation on or
    /// off as the cell said. Counters, every `..._ms` a sum over the traversal:
    /// `prompt_ms` / `predicted_ms` on the runtime's clock, `ttft_ms` /
    /// `decode_ms` / `e2e_ms` on the client's around the request. A basis:
    /// target forward passes are `generated - accepted`, so not carried.
    VlSpecDecode {
        /// Turns the traversal covered. The server divides by it, so it must
        /// be `> 0`.
        vl_spec_decode_turns: u32,
        /// The runtime's own prefill time, summed: the reference harness's
        /// TTFT, and with `predicted_ms` its e2e; comparable within one engine.
        vl_spec_decode_prompt_ms: f64,
        vl_spec_decode_predicted_ms: f64,
        /// Client clock, send to first token.
        vl_spec_decode_ttft_ms: f64,
        /// Client clock, first token to last token.
        vl_spec_decode_decode_ms: f64,
        /// Client clock, send to the end of the stream. Must be `> 0` and at
        /// least `ttft + decode`.
        vl_spec_decode_e2e_ms: f64,
        /// With speculation on, always `>` the accepted-token count, so the
        /// accept length's denominator is positive.
        vl_spec_decode_generated_tokens: u64,
        /// Tokens the drafter proposed, and those the target accepted. Present
        /// exactly when speculation was on: zeros would put a `0 / 0` in the
        /// server's acceptance rate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vl_spec_decode_drafted_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vl_spec_decode_accepted_tokens: Option<u64>,
        /// Σ over turns of `generated / (generated - accepted)`, present
        /// exactly when speculation was on. Over `turns`, the request-mean
        /// accept length (SGLang's `spec_accept_length`), which the pooled
        /// counters cannot give.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vl_spec_decode_turn_accept_length_sum: Option<f64>,
    },
}

impl BenchmarkResultData {
    /// The kind most variants imply: a shape hint, never the authoritative
    /// type. Not one-to-one: `vl_max_memory` and `asr_max_memory` reuse
    /// `MaxMemoryUsage`, and the `Eval { completions }` shape is shared by
    /// `eval`, `vlm_eval` and `asr_eval` and reads as `Eval`. Take the true type
    /// from the benchmark definition.
    pub fn benchmark_type(&self) -> BenchmarkType {
        match self {
            Self::PrefillThroughput { .. } => BenchmarkType::PrefillThroughput,
            Self::DecodeThroughput { .. } => BenchmarkType::DecodeThroughput,
            Self::EndToEndLatency { .. } => BenchmarkType::EndToEndLatency,
            Self::MaxMemoryUsage { .. } => BenchmarkType::MaxMemoryUsage,
            Self::Eval { .. } => BenchmarkType::Eval,
            Self::VlThroughput { .. } => BenchmarkType::VlThroughput,
            Self::VlSpecDecode { .. } => BenchmarkType::VlSpecDecode,
            Self::AsrThroughput { .. } => BenchmarkType::AsrThroughput,
        }
    }
}

/// Payload a device submits to the management server as a result.
///
/// Flattens five types and [`declared_benchmark_params`]: every field name
/// across them must be unique, or deserialization silently breaks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkSubmissionPayload {
    pub benchmark_id: String,
    /// What the client ran, which the server checks against its catalog. Wire
    /// form: [`declared_benchmark_params`].
    #[serde(flatten, with = "declared_benchmark_params")]
    pub benchmark_params: Option<BenchmarkParams>,
    #[serde(flatten)]
    pub device: DeviceInfo,
    // Optional: a desktop with no battery, or a platform that can't read it,
    // omits them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_battery_level: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_power_state: Option<DevicePowerState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_power_save_mode: Option<bool>,
    // Flattened so each `device_*_thermal_*` field lands at the top level, as
    // the warehouse columns are.
    #[serde(flatten)]
    pub thermal: ThermalTelemetry,
    // Flattened beside thermal and power: conditions the result was measured
    // under, not the result.
    #[serde(flatten)]
    pub memory: MemoryObservation,
    /// Canonical JSON of the run's `pipette_plan_types::Model`, the lossless
    /// model coordinate; the server re-canonicalizes and stores it opaquely.
    /// Defaults to empty so an older on-disk payload still loads;
    /// `submit_pending_result` refuses to send one.
    #[serde(default)]
    pub model_descriptor: String,
    /// Canonical JSON of the run's `pipette_plan_types::Runtime`, the lossless
    /// runtime and build identity; load knobs ship as `runtime_flags`.
    #[serde(default)]
    pub runtime_descriptor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_flags: Option<String>,
    /// Canonical JSON of the run's [`RuntimeFlags`](crate::RuntimeFlags) in its
    /// flat wire form: the load knobs the cell ran with, plan entry plus what
    /// the client derived. Env forwards carry names only, `GGML_*` aside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_flags: Option<String>,
    /// Canonical JSON of what the *harness* ran under (readiness gating,
    /// timeouts, loop detection), as
    /// [`BenchmarkFlags::submission_value`](crate::BenchmarkFlags::submission_value)
    /// builds it. Resolved, not authored: the server's only record of
    /// readiness, such as a waived thermal gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_flags: Option<String>,
    /// Canonical JSON of the
    /// [`ExtraRuntimeConfiguration`](crate::ExtraRuntimeConfiguration) the
    /// client ran this runtime under, as
    /// [`ExtraRuntimeConfiguration::submission_value`](crate::ExtraRuntimeConfiguration::submission_value)
    /// builds it. Beside `runtime_flags`, never inside: it is no part of the
    /// cell, only a measurement condition. `None` when the client had no entry
    /// for the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_configuration: Option<String>,
    /// What the device's sensors reported around each measured rep: a JSON
    /// array in a string, one element per rep and sample point, each facility
    /// under its own key. Opaque to the server, which keeps it cut to
    /// [`MAX_DEVICE_READINGS_BYTES`] by [`keep_last_elements`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_hardware_readings: Option<String>,
    /// What the OS or firmware reported about its own throttling around each
    /// measured rep (states, levels, bitmasks), in the shape of
    /// `device_hardware_readings`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_throttling_readings: Option<String>,
    /// What the readiness gate reported before each measured rep: a JSON array
    /// in a string, one element per rep with its `iteration`, `poll_count` and
    /// the gate's report of its last poll. Cut to
    /// [`MAX_READINESS_GATE_WAITS_BYTES`] the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_gate_waits: Option<String>,
    /// Runtime-selected CPU kernel variant, per `runtime_name`: for llama.cpp
    /// the CPU backend chosen at load by feature-dispatch scoring. `None` for a
    /// single static CPU backend (e.g. iOS and desktop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_cpu_variant: Option<String>,
    /// The sha256 the client computed over the corpus tar it downloaded, for a
    /// benchmark type that serves one. Sent even when it does not match the
    /// catalog's pin: the server records the verdict. `None` for a run that
    /// pulled no corpus. Not namespaced by type, like the server's column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_dataset_digest: Option<Sha256>,
    /// sha256 over a corpus traversal's replies, in order, each turn as
    /// `sha256(text)`, a tab, its token count, a newline. Equal digests mean
    /// greedy-identical output without either row's text leaving the device.
    /// `None` for every type that traverses no corpus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_output_digest: Option<Sha256>,
    /// Version of the client build that produced this run, not the runtime
    /// (`runtime_descriptor`): a grouping key that attributes a shift in the
    /// numbers to a harness change. `None` only on an older on-disk payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// The milliseconds that the device spent on the claimed job; `None` if
    /// not measured. The server refuses a value past `i64::MAX`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_time_ms: Option<u64>,
    /// Anything the client reports that no other field names, as a JSON object
    /// in a string of at most [`MAX_CLIENT_EXTRAS_BYTES`]. Stored as sent;
    /// nothing groups, matches or scores on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_extras: Option<String>,
    #[serde(flatten)]
    pub runtime_output: RuntimeOutput,
    pub submitted_at: String,
    #[serde(flatten)]
    pub result: BenchmarkResultData,
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use super::*;
    use crate::ModelFlags;
    use crate::PositiveCount;
    use crate::benchmark::BatchSize;
    use crate::device::DeviceFormFactor;
    use crate::thermal::{LinuxThermalZone, ThermalReading};

    fn observed(host: Option<u64>, swap: Option<u64>) -> MemoryObservation {
        MemoryObservation {
            max_host_bytes: host,
            max_swap_bytes: swap,
        }
    }

    /// Folding across repetitions must keep the worst of each term and must never
    /// let "not observed" beat a real reading.
    #[rstest]
    // The worst rep wins, not the last one.
    #[case::worst_rep_wins(
        observed(Some(600), Some(50)),
        observed(Some(400), Some(10)),
        observed(Some(600), Some(50))
    )]
    // Terms are taken independently: the biggest peak and the biggest swap can
    // come from different reps, and picking one rep wholesale would lose one.
    #[case::terms_are_independent(
        observed(Some(600), Some(10)),
        observed(Some(400), Some(90)),
        observed(Some(600), Some(90))
    )]
    // `None` is "nobody looked", so a rep that observed nothing must not erase a
    // rep that did.
    #[case::absence_never_wins(
        observed(Some(600), Some(50)),
        observed(None, None),
        observed(Some(600), Some(50))
    )]
    #[case::absence_never_wins_either_order(
        observed(None, None),
        observed(Some(600), Some(50)),
        observed(Some(600), Some(50))
    )]
    // A sampled zero is a real reading and survives, unlike absence.
    #[case::a_sampled_zero_is_kept(
        observed(Some(600), Some(0)),
        observed(None, None),
        observed(Some(600), Some(0))
    )]
    #[case::nothing_observed_stays_empty(
        observed(None, None),
        observed(None, None),
        observed(None, None)
    )]
    fn merging_reps_keeps_the_worst_of_each_term(
        #[case] a: MemoryObservation,
        #[case] b: MemoryObservation,
        #[case] expected: MemoryObservation,
    ) {
        assert_eq!(a.merge_max(b), expected);
    }

    /// The constructors are where the "zero is not a measurement" rule lives, so
    /// every arm that builds an observation inherits it.
    #[rstest]
    #[case::host_only_keeps_a_real_peak(
        observed(Some(600), None),
        MemoryObservation::host_only(600)
    )]
    #[case::host_only_withholds_a_zero(observed(None, None), MemoryObservation::host_only(0))]
    // A sampled zero swap is a reading and survives beside a real peak.
    #[case::swap_zero_is_a_reading(
        observed(Some(600), Some(0)),
        MemoryObservation::with_swap(600, 0)
    )]
    #[case::swap_rides_with_the_peak(
        observed(Some(600), Some(50)),
        MemoryObservation::with_swap(600, 50)
    )]
    // Never half an observation: no peak means the swap term goes too.
    #[case::a_withheld_peak_withholds_swap(
        observed(None, None),
        MemoryObservation::with_swap(0, 50)
    )]
    fn a_zero_peak_is_withheld_rather_than_reported(
        #[case] expected: MemoryObservation,
        #[case] built: MemoryObservation,
    ) {
        assert_eq!(built, expected);
    }

    fn parses<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> bool {
        serde_json::from_value::<T>(value).is_ok()
    }

    /// `1e300` is how an overflowing literal arrives: `serde_json` reads it
    /// into an `f32` as `inf`.
    #[rstest]
    #[case::positive_millis(parses::<PositiveMillis>, serde_json::json!(0.5), true)]
    #[case::positive_millis_zero(parses::<PositiveMillis>, serde_json::json!(0.0), false)]
    #[case::positive_millis_negative(parses::<PositiveMillis>, serde_json::json!(-1.0), false)]
    #[case::positive_millis_overflow(parses::<PositiveMillis>, serde_json::json!(1e300), false)]
    #[case::non_negative_millis(parses::<NonNegativeMillis>, serde_json::json!(0.5), true)]
    #[case::non_negative_millis_zero(parses::<NonNegativeMillis>, serde_json::json!(0.0), true)]
    #[case::non_negative_millis_negative(
        parses::<NonNegativeMillis>,
        serde_json::json!(-1.0),
        false
    )]
    #[case::non_negative_millis_overflow(
        parses::<NonNegativeMillis>,
        serde_json::json!(1e300),
        false
    )]
    #[case::byte_count_zero(parses::<ByteCount>, serde_json::json!(0), true)]
    #[case::byte_count_max(parses::<ByteCount>, serde_json::json!(i64::MAX), true)]
    #[case::byte_count_negative(parses::<ByteCount>, serde_json::json!(-1), false)]
    fn range_bounds(
        #[case] parses: fn(serde_json::Value) -> bool,
        #[case] value: serde_json::Value,
        #[case] ok: bool,
    ) {
        assert_eq!(parses(value.clone()), ok, "{value}");
    }

    #[rstest]
    #[case::zero(0, Ok(0))]
    #[case::max(i64::MAX.unsigned_abs(), Ok(i64::MAX))]
    #[case::past_max(
        1 << 63,
        Err("byte count must be at most 9223372036854775807, got 9223372036854775808")
    )]
    fn byte_count_from_a_u64(#[case] bytes: u64, #[case] expected: Result<i64, &str>) {
        let converted = ByteCount::try_from(bytes)
            .map(ByteCount::into_inner)
            .map_err(|e| e.to_string());
        assert_eq!(converted, expected.map_err(str::to_owned));
    }

    /// An observation with nothing in it must contribute no keys, so a client on
    /// a platform without a sampler leaves the wire shape untouched.
    #[test]
    fn an_empty_observation_serializes_to_nothing() -> anyhow::Result<()> {
        let json = serde_json::to_value(MemoryObservation::default())?;
        assert_eq!(json, serde_json::json!({}), "got {json}");
        Ok(())
    }

    /// Every untagged variant survives a round trip, with and without its
    /// optional stddev, where the numeric variants' field sets are closest.
    #[rstest]
    #[case::prefill(BenchmarkResultData::PrefillThroughput {
        prefill_time_ms: 12.5, prefill_time_ms_stddev: None,
    })]
    #[case::prefill_stddev(BenchmarkResultData::PrefillThroughput {
        prefill_time_ms: 12.5, prefill_time_ms_stddev: Some(0.4),
    })]
    #[case::decode(BenchmarkResultData::DecodeThroughput {
        decode_time_ms: 8.25, decode_time_ms_stddev: None,
    })]
    #[case::decode_stddev(BenchmarkResultData::DecodeThroughput {
        decode_time_ms: 8.25, decode_time_ms_stddev: Some(0.2),
    })]
    #[case::end_to_end(BenchmarkResultData::EndToEndLatency {
        total_time_ms: 940.0, total_time_ms_stddev: None,
    })]
    #[case::end_to_end_stddev(BenchmarkResultData::EndToEndLatency {
        total_time_ms: 940.0, total_time_ms_stddev: Some(11.0),
    })]
    #[case::max_memory(BenchmarkResultData::MaxMemoryUsage {
        max_host_bytes: 8_589_934_592, max_gpu_bytes: None, max_npu_bytes: None,
    })]
    #[case::max_memory_all_peaks(BenchmarkResultData::MaxMemoryUsage {
        max_host_bytes: 8_589_934_592,
        max_gpu_bytes: Some(4_294_967_296),
        max_npu_bytes: Some(1_073_741_824),
    })]
    #[case::eval(BenchmarkResultData::Eval {
        completions: vec![BenchmarkEvalCompletion { id: "s1".into(), ..BenchmarkEvalCompletion::default() }],
    })]
    #[case::vl(BenchmarkResultData::VlThroughput {
        prompt_tokens: 75, image_tokens: None, prompt_ms: 352.3, prompt_ms_stddev: None,
        predicted_ms: 32.7, predicted_ms_stddev: None, total_time_ms: None, total_time_ms_stddev: None,
    })]
    #[case::vl_stddev(BenchmarkResultData::VlThroughput {
        prompt_tokens: 75, image_tokens: Some(64), prompt_ms: 352.3, prompt_ms_stddev: Some(3.8),
        predicted_ms: 32.7, predicted_ms_stddev: Some(1.2), total_time_ms: None, total_time_ms_stddev: None,
    })]
    // Would decode as `EndToEndLatency` were that variant first.
    #[case::vl_total(BenchmarkResultData::VlThroughput {
        prompt_tokens: 75, image_tokens: Some(64), prompt_ms: 352.3, prompt_ms_stddev: None,
        predicted_ms: 32.7, predicted_ms_stddev: None, total_time_ms: Some(394.1), total_time_ms_stddev: None,
    })]
    #[case::vl_total_stddev(BenchmarkResultData::VlThroughput {
        prompt_tokens: 75, image_tokens: Some(64), prompt_ms: 352.3, prompt_ms_stddev: Some(3.8),
        predicted_ms: 32.7, predicted_ms_stddev: Some(1.2), total_time_ms: Some(394.1),
        total_time_ms_stddev: Some(4.2),
    })]
    // The plain ASR row: its fields are all `asr_*`, so it shares no field
    // name with any other variant; the case after it is the one order guards.
    #[case::asr(BenchmarkResultData::AsrThroughput {
        asr_prompt_tokens: 137, asr_audio_tokens: 118, asr_prompt_ms: 83.3,
        asr_prompt_ms_stddev: None, asr_predicted_ms: 129.4, asr_predicted_ms_stddev: None,
        total_time_ms: None, total_time_ms_stddev: None,
    })]
    // With the client's total too: still ASR, never VL or end-to-end latency.
    #[case::asr_stddev(BenchmarkResultData::AsrThroughput {
        asr_prompt_tokens: 137, asr_audio_tokens: 118, asr_prompt_ms: 83.3,
        asr_prompt_ms_stddev: Some(0.9), asr_predicted_ms: 129.4, asr_predicted_ms_stddev: Some(1.1),
        total_time_ms: Some(230.0), total_time_ms_stddev: Some(2.0),
    })]
    // Every field is `vl_spec_decode_*`-prefixed, so the untagged enum
    // cannot mistake this payload for an earlier variant's. Both modes: the
    // drafter counters are absent with speculation off, not zero.
    #[case::vl_spec_decode_off(BenchmarkResultData::VlSpecDecode {
        vl_spec_decode_turns: 723,
        vl_spec_decode_prompt_ms: 190_884.1,
        vl_spec_decode_predicted_ms: 908_442.7,
        vl_spec_decode_ttft_ms: 209_972.4,
        vl_spec_decode_decode_ms: 915_106.3,
        vl_spec_decode_e2e_ms: 1_132_308.0,
        vl_spec_decode_generated_tokens: 56_402,
        vl_spec_decode_drafted_tokens: None,
        vl_spec_decode_accepted_tokens: None,
        vl_spec_decode_turn_accept_length_sum: None,
    })]
    #[case::vl_spec_decode_on(BenchmarkResultData::VlSpecDecode {
        vl_spec_decode_turns: 723,
        vl_spec_decode_prompt_ms: 191_233.4,
        vl_spec_decode_predicted_ms: 402_881.2,
        vl_spec_decode_ttft_ms: 210_356.7,
        vl_spec_decode_decode_ms: 407_712.9,
        vl_spec_decode_e2e_ms: 625_299.1,
        vl_spec_decode_generated_tokens: 56_431,
        vl_spec_decode_drafted_tokens: Some(113_032),
        vl_spec_decode_accepted_tokens: Some(42_302),
        vl_spec_decode_turn_accept_length_sum: Some(3_036.6),
    })]
    fn result_data_untagged_variant_round_trips(
        #[case] data: BenchmarkResultData,
    ) -> anyhow::Result<()> {
        let round_tripped: BenchmarkResultData =
            serde_json::from_value(serde_json::to_value(&data)?)?;
        assert_eq!(round_tripped, data);
        Ok(())
    }

    /// Without the total, the wire shape is unchanged.
    #[test]
    fn a_vl_result_without_a_total_keeps_its_wire_shape() -> anyhow::Result<()> {
        let data = BenchmarkResultData::VlThroughput {
            prompt_tokens: 75,
            image_tokens: None,
            prompt_ms: 352.3,
            prompt_ms_stddev: None,
            predicted_ms: 32.7,
            predicted_ms_stddev: None,
            total_time_ms: None,
            total_time_ms_stddev: None,
        };
        let old_body = serde_json::json!({
            "prompt_tokens": 75, "prompt_ms": 352.3, "predicted_ms": 32.7,
        });
        assert_eq!(serde_json::to_value(&data)?, old_body);
        let parsed: BenchmarkResultData = serde_json::from_value(old_body)?;
        assert_eq!(parsed, data);
        Ok(())
    }

    /// Each variant reports the kind its shape implies. The shared shapes read
    /// as one kind: `MaxMemoryUsage` also carries `vl_max_memory`, and `Eval`
    /// also carries `vlm_eval` and `asr_eval`.
    #[rstest]
    #[case::prefill(BenchmarkResultData::PrefillThroughput {
        prefill_time_ms: 1.0, prefill_time_ms_stddev: None,
    }, BenchmarkType::PrefillThroughput)]
    #[case::decode(BenchmarkResultData::DecodeThroughput {
        decode_time_ms: 1.0, decode_time_ms_stddev: None,
    }, BenchmarkType::DecodeThroughput)]
    #[case::end_to_end(BenchmarkResultData::EndToEndLatency {
        total_time_ms: 1.0, total_time_ms_stddev: None,
    }, BenchmarkType::EndToEndLatency)]
    #[case::max_memory(BenchmarkResultData::MaxMemoryUsage {
        max_host_bytes: 1, max_gpu_bytes: None, max_npu_bytes: None,
    }, BenchmarkType::MaxMemoryUsage)]
    #[case::eval(BenchmarkResultData::Eval { completions: vec![] }, BenchmarkType::Eval)]
    #[case::vl(BenchmarkResultData::VlThroughput {
        prompt_tokens: 1, image_tokens: None, prompt_ms: 1.0, prompt_ms_stddev: None,
        predicted_ms: 1.0, predicted_ms_stddev: None, total_time_ms: None, total_time_ms_stddev: None,
    }, BenchmarkType::VlThroughput)]
    #[case::asr(BenchmarkResultData::AsrThroughput {
        asr_prompt_tokens: 1, asr_audio_tokens: 1, asr_prompt_ms: 1.0,
        asr_prompt_ms_stddev: None, asr_predicted_ms: 1.0, asr_predicted_ms_stddev: None,
        total_time_ms: None, total_time_ms_stddev: None,
    }, BenchmarkType::AsrThroughput)]
    #[case::vl_spec_decode(BenchmarkResultData::VlSpecDecode {
        vl_spec_decode_turns: 1, vl_spec_decode_prompt_ms: 1.0,
        vl_spec_decode_predicted_ms: 1.0,
        vl_spec_decode_ttft_ms: 1.0, vl_spec_decode_decode_ms: 1.0,
        vl_spec_decode_e2e_ms: 1.0, vl_spec_decode_generated_tokens: 1,
        vl_spec_decode_drafted_tokens: None, vl_spec_decode_accepted_tokens: None,
        vl_spec_decode_turn_accept_length_sum: None,
    }, BenchmarkType::VlSpecDecode)]
    fn benchmark_type_matches_the_variant(
        #[case] data: BenchmarkResultData,
        #[case] expected: BenchmarkType,
    ) {
        assert_eq!(data.benchmark_type(), expected);
    }

    // The struct serializes to the JSON and the JSON parses back: `stop_reason`
    // is always written, the other optional fields elide when unset.
    #[rstest]
    #[case::non_failed_carries_stop_reason(
        BenchmarkEvalCompletion {
            id: "s1".into(), completion: "answer".into(), failed: false, failed_reason: None,
            stop_reason: BenchmarkEvalCompletionStopReason::Eos, stop_detail: None, completion_tokens: None,
        },
        serde_json::json!({"id": "s1", "completion": "answer", "stop_reason": "eos"}),
    )]
    #[case::failed_marker_round_trips(
        BenchmarkEvalCompletion {
            id: "s2".into(), completion: String::new(), failed: true,
            failed_reason: Some("stack overflow".into()), stop_reason: BenchmarkEvalCompletionStopReason::Failure,
            stop_detail: Some("stack overflow".into()), completion_tokens: None,
        },
        serde_json::json!({
            "id": "s2", "completion": "", "failed": true, "failed_reason": "stack overflow",
            "stop_reason": "failure", "stop_detail": "stack overflow",
        }),
    )]
    #[case::stop_reason_and_tokens_round_trip(
        BenchmarkEvalCompletion {
            id: "s3".into(), completion: "truncated output".into(), failed: false,
            failed_reason: None, stop_reason: BenchmarkEvalCompletionStopReason::Truncated, stop_detail: None,
            completion_tokens: Some(8192),
        },
        serde_json::json!({
            "id": "s3", "completion": "truncated output", "stop_reason": "truncated",
            "completion_tokens": 8192,
        }),
    )]
    #[case::unknown_carries_stop_detail(
        BenchmarkEvalCompletion {
            id: "s4".into(), completion: "partial".into(), failed: false, failed_reason: None,
            stop_reason: BenchmarkEvalCompletionStopReason::Unknown,
            stop_detail: Some("stream ended without a terminal stop event".into()),
            completion_tokens: None,
        },
        serde_json::json!({
            "id": "s4", "completion": "partial", "stop_reason": "unknown",
            "stop_detail": "stream ended without a terminal stop event",
        }),
    )]
    fn benchmark_eval_completion_serde_contract(
        #[case] canonical: BenchmarkEvalCompletion,
        #[case] expected_json: serde_json::Value,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_value(&canonical)?;
        assert_eq!(serialized, expected_json, "serialized shape");
        let parsed: BenchmarkEvalCompletion = serde_json::from_value(expected_json.clone())?;
        assert_eq!(parsed, canonical, "parsed value");
        Ok(())
    }

    #[test]
    fn pre_feature_payload_parses_stop_reason_as_unknown() -> anyhow::Result<()> {
        let legacy: BenchmarkEvalCompletion =
            serde_json::from_value(serde_json::json!({"id": "old", "completion": "x"}))?;
        assert_eq!(
            legacy.stop_reason,
            BenchmarkEvalCompletionStopReason::Unknown
        );
        Ok(())
    }

    #[test]
    fn legacy_doom_loop_row_still_parses() -> anyhow::Result<()> {
        let legacy: BenchmarkEvalCompletion = serde_json::from_value(serde_json::json!({
            "id": "old", "completion": "x", "stop_reason": "doom_loop"
        }))?;
        assert_eq!(
            legacy.stop_reason,
            BenchmarkEvalCompletionStopReason::DoomLoop
        );
        assert_eq!(serde_json::to_value(&legacy)?["stop_reason"], "doom_loop");
        Ok(())
    }

    #[test]
    fn max_memory_usage_serializes_with_legacy_wire_field_names() -> anyhow::Result<()> {
        // mgmt's submission handler reads `max_ram_bytes` / `max_vram_bytes`.
        let data = BenchmarkResultData::MaxMemoryUsage {
            max_host_bytes: 452_755_456,
            max_gpu_bytes: Some(123_456_789),
            max_npu_bytes: None,
        };
        let json = serde_json::to_value(&data)?;
        assert_eq!(json["max_ram_bytes"].as_u64(), Some(452_755_456));
        assert_eq!(json["max_vram_bytes"].as_u64(), Some(123_456_789));
        assert!(
            json.get("max_host_bytes").is_none(),
            "must not emit `max_host_bytes` on the wire: {json}"
        );
        assert!(
            json.get("max_gpu_bytes").is_none(),
            "must not emit `max_gpu_bytes` on the wire: {json}"
        );
        Ok(())
    }

    #[test]
    fn max_memory_usage_deserializes_both_legacy_and_methodology_field_names() -> anyhow::Result<()>
    {
        // The enum is untagged, so the field names alone pick the variant.
        let legacy = serde_json::json!({
            "max_ram_bytes": 1_000_000_u64,
            "max_vram_bytes": 500_000_u64
        });
        let from_legacy: BenchmarkResultData = serde_json::from_value(legacy)?;
        let methodology = serde_json::json!({
            "max_host_bytes": 1_000_000_u64,
            "max_gpu_bytes": 500_000_u64
        });
        let from_methodology: BenchmarkResultData = serde_json::from_value(methodology)?;
        assert_eq!(from_legacy, from_methodology);
        match from_legacy {
            BenchmarkResultData::MaxMemoryUsage {
                max_host_bytes,
                max_gpu_bytes,
                max_npu_bytes,
            } => {
                assert_eq!(max_host_bytes, 1_000_000);
                assert_eq!(max_gpu_bytes, Some(500_000));
                assert_eq!(max_npu_bytes, None);
            }
            other @ (BenchmarkResultData::PrefillThroughput { .. }
            | BenchmarkResultData::DecodeThroughput { .. }
            | BenchmarkResultData::EndToEndLatency { .. }
            | BenchmarkResultData::Eval { .. }
            | BenchmarkResultData::VlThroughput { .. }
            | BenchmarkResultData::AsrThroughput { .. }
            | BenchmarkResultData::VlSpecDecode { .. }) => {
                anyhow::bail!("expected MaxMemoryUsage, got {other:?}")
            }
        }
        Ok(())
    }

    // ---- BenchmarkSubmissionPayload: the flattened wire shape ----

    // Opaque here; the runtime crates assert the real `Model`/`Runtime` JSON.
    const MODEL_DESC: &str = r#"{"type":"gguf_text","source":"huggingface","org":"o","repo_name":"r","path":"m-Q4_0.gguf"}"#;
    const RT_DESC: &str = r#"{"type":"llamacpp_cli_stock_tools","source":"repository"}"#;
    const CLIENT_CONFIG: &str = r#"{"envs":["CUDA_VISIBLE_DEVICES"]}"#;

    fn sample_payload() -> BenchmarkSubmissionPayload {
        BenchmarkSubmissionPayload {
            benchmark_id: "decode_throughput_256_128".to_string(),
            benchmark_params: None,
            device: DeviceInfo {
                device_name: "test".into(),
                device_form_factor: DeviceFormFactor::Embedded,
                device_os_name: "Linux".into(),
                device_os_version: "22.04".into(),
                device_os_build: None,
                device_os_security_patch: None,
                device_cpu_arch: None,
                device_os_family: None,
                device_chip_model: "test".into(),
                device_ram_bytes: 0,
                device_gpu_model: None,
                device_gpu_vram_bytes: None,
                device_npu_model: None,
                device_npu_vram_bytes: None,
            },
            device_battery_level: None,
            device_power_state: None,
            device_power_save_mode: None,
            thermal: ThermalTelemetry::default(),
            memory: MemoryObservation::default(),
            model_descriptor: MODEL_DESC.to_string(),
            runtime_descriptor: RT_DESC.to_string(),
            model_flags: None,
            runtime_flags: None,
            benchmark_flags: None,
            client_configuration: None,
            device_hardware_readings: None,
            device_os_throttling_readings: None,
            readiness_gate_waits: None,
            runtime_cpu_variant: None,
            observation_dataset_digest: None,
            observation_output_digest: None,
            client_version: None,
            device_time_ms: None,
            client_extras: None,
            runtime_output: RuntimeOutput::default(),
            submitted_at: "2026-01-01T00:00:00Z".to_string(),
            result: BenchmarkResultData::DecodeThroughput {
                decode_time_ms: 50.0,
                decode_time_ms_stddev: None,
            },
        }
    }

    #[test]
    fn submission_payload_flattens_device_info() -> anyhow::Result<()> {
        let payload = BenchmarkSubmissionPayload {
            benchmark_id: "prefill_throughput_256".to_string(),
            benchmark_params: None,
            device: DeviceInfo {
                device_name: "Test".into(),
                device_form_factor: DeviceFormFactor::Laptop,
                device_os_name: "Linux".into(),
                device_os_version: "22.04".into(),
                device_os_build: None,
                device_os_security_patch: None,
                device_cpu_arch: None,
                device_os_family: None,
                device_chip_model: "x86_64".into(),
                device_ram_bytes: 8_000_000_000,
                device_gpu_model: None,
                device_gpu_vram_bytes: None,
                device_npu_model: None,
                device_npu_vram_bytes: None,
            },
            device_battery_level: None,
            device_power_state: None,
            device_power_save_mode: None,
            thermal: ThermalTelemetry::default(),
            memory: MemoryObservation::default(),
            model_descriptor: MODEL_DESC.to_string(),
            runtime_descriptor: RT_DESC.to_string(),
            model_flags: None,
            runtime_flags: None,
            benchmark_flags: None,
            client_configuration: None,
            device_hardware_readings: None,
            device_os_throttling_readings: None,
            readiness_gate_waits: None,
            runtime_cpu_variant: None,
            observation_dataset_digest: None,
            observation_output_digest: None,
            client_version: None,
            device_time_ms: None,
            client_extras: None,
            runtime_output: RuntimeOutput::default(),
            submitted_at: "2026-01-01T00:00:00Z".to_string(),
            result: BenchmarkResultData::PrefillThroughput {
                prefill_time_ms: 34.7,
                prefill_time_ms_stddev: None,
            },
        };
        let value = serde_json::to_value(&payload)?;
        assert_eq!(value["device_name"], "Test");
        assert_eq!(value["device_form_factor"], "laptop");
        assert_eq!(value["device_os_name"], "Linux");
        assert_eq!(value["device_ram_bytes"], 8_000_000_000u64);
        assert_eq!(value["benchmark_id"], "prefill_throughput_256");
        Ok(())
    }

    #[test]
    fn old_payload_without_device_fields_still_deserializes() -> anyhow::Result<()> {
        // `#[serde(default)]` on DeviceInfo fills the missing device fields.
        let old_json = serde_json::json!({
            "benchmark_id": "prefill_throughput_256",
            "hardware_details": "mac-studio-m3-ultra-96gb",
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "submitted_at": "2026-01-01T00:00:00Z",
            "prefill_time_ms": 34.7
        });
        let payload: BenchmarkSubmissionPayload = serde_json::from_value(old_json)?;
        assert_eq!(payload.benchmark_id, "prefill_throughput_256");
        assert_eq!(payload.device.device_name.as_ref(), "");
        assert_eq!(payload.device.device_ram_bytes, 0);
        assert_eq!(payload.benchmark_params, None);
        Ok(())
    }

    #[test]
    fn submission_payload_round_trip_with_device_info() -> anyhow::Result<()> {
        let payload = BenchmarkSubmissionPayload {
            benchmark_id: "decode_throughput_256_128".to_string(),
            benchmark_params: Some(BenchmarkParams::DecodeThroughput {
                prefill_tokens: crate::NonNegativeCount::try_new(256)?,
                decode_tokens: PositiveCount::try_new(128)?,
                batch_size: BatchSize::one(),
            }),
            device: DeviceInfo {
                device_name: "Jetson Orin Nano 8GB".into(),
                device_form_factor: DeviceFormFactor::Embedded,
                device_os_name: "Linux".into(),
                device_os_version: "22.04".into(),
                device_os_build: Some("6.8.0-45-generic".into()),
                device_os_security_patch: Some("2025-06-01".into()),
                device_cpu_arch: Some("aarch64".into()),
                device_os_family: Some("Linux".into()),
                device_chip_model: "NVIDIA Jetson Orin Nano".into(),
                device_ram_bytes: 8_589_934_592,
                device_gpu_model: None,
                device_gpu_vram_bytes: None,
                device_npu_model: None,
                device_npu_vram_bytes: None,
            },
            device_battery_level: None,
            device_power_state: None,
            device_power_save_mode: None,
            // Two reps exercise the iteration-tagged lists; `from_series`
            // stamps `iteration`, so the zeros below are placeholders.
            thermal: ThermalTelemetry::from_series(
                &[
                    ThermalReading {
                        linux_thermal_zones: Some(vec![
                            LinuxThermalZone {
                                iteration: 0,
                                zone_type: "cpu-thermal".into(),
                                celsius: 44,
                            },
                            LinuxThermalZone {
                                iteration: 0,
                                zone_type: "gpu-thermal".into(),
                                celsius: 41,
                            },
                        ]),
                        ..Default::default()
                    },
                    ThermalReading {
                        linux_thermal_zones: Some(vec![
                            LinuxThermalZone {
                                iteration: 0,
                                zone_type: "cpu-thermal".into(),
                                celsius: 55,
                            },
                            LinuxThermalZone {
                                iteration: 0,
                                zone_type: "gpu-thermal".into(),
                                celsius: 50,
                            },
                        ]),
                        ..Default::default()
                    },
                ],
                &[ThermalReading {
                    linux_thermal_zones: Some(vec![
                        LinuxThermalZone {
                            iteration: 0,
                            zone_type: "cpu-thermal".into(),
                            celsius: 63,
                        },
                        LinuxThermalZone {
                            iteration: 0,
                            zone_type: "gpu-thermal".into(),
                            celsius: 58,
                        },
                    ]),
                    ..Default::default()
                }],
            ),
            memory: MemoryObservation {
                max_host_bytes: Some(6_594_494_464),
                max_swap_bytes: Some(356_515_840),
            },
            model_descriptor: MODEL_DESC.to_string(),
            runtime_descriptor: RT_DESC.to_string(),
            model_flags: None,
            runtime_flags: None,
            benchmark_flags: None,
            // Opaque here, like the descriptors: it only has to survive the
            // flatten round trip.
            client_configuration: Some(CLIENT_CONFIG.to_string()),
            device_hardware_readings: None,
            device_os_throttling_readings: None,
            readiness_gate_waits: None,
            runtime_cpu_variant: None,
            observation_dataset_digest: None,
            observation_output_digest: None,
            client_version: None,
            device_time_ms: Some(184_250),
            client_extras: Some(r#"{"build":"nightly"}"#.to_string()),
            // An empty stream is a capture of nothing, so it must survive as
            // `""` rather than collapse to absent.
            runtime_output: RuntimeOutput {
                runtime_stdout: Some("loaded model\n".to_string()),
                runtime_stderr: Some(String::new()),
            },
            submitted_at: "2026-01-01T00:00:00Z".to_string(),
            result: BenchmarkResultData::DecodeThroughput {
                decode_time_ms: 50.0,
                decode_time_ms_stddev: Some(2.1),
            },
        };
        let json = serde_json::to_value(&payload)?;
        // Flattened at the top level (not nested under `thermal`), matching the
        // warehouse columns.
        assert_eq!(
            json["device_linux_thermal_zones_before"][0]["type"],
            "cpu-thermal"
        );
        assert!(json.get("thermal").is_none());
        // The memory observation flattens too, on a decode row: it rides on
        // every benchmark, not just the memory one.
        assert_eq!(json["observation_max_host_bytes"], 6_594_494_464u64);
        assert_eq!(json["observation_max_swap_bytes"], 356_515_840u64);
        assert!(
            json.get("memory").is_none(),
            "must flatten, not nest: {json}"
        );
        assert_eq!(json["client_configuration"], CLIENT_CONFIG);
        assert_eq!(json["runtime_stdout"], "loaded model\n");
        assert_eq!(json["runtime_stderr"], "");
        assert_eq!(json["device_time_ms"], 184_250);
        assert!(
            json.get("runtime_output").is_none(),
            "must flatten, not nest: {json}"
        );
        let round_tripped: BenchmarkSubmissionPayload = serde_json::from_value(json)?;
        assert_eq!(round_tripped, payload);
        Ok(())
    }

    /// One case per kind. The zeros are the catalog's own empty prompts.
    #[rstest]
    #[case::prefill_throughput(
        "prefill_throughput",
        serde_json::json!({"prefill_tokens": 256, "batch_size": 1})
    )]
    #[case::decode_throughput(
        "decode_throughput",
        serde_json::json!({"prefill_tokens": 0, "decode_tokens": 32, "batch_size": 1})
    )]
    // Same fields as `decode_throughput`: only the top-level type tells them apart.
    #[case::end_to_end_latency(
        "end_to_end_latency",
        serde_json::json!({"prefill_tokens": 256, "decode_tokens": 128, "batch_size": 1})
    )]
    #[case::max_memory_usage("max_memory_usage", serde_json::json!({"prefill_tokens": 256}))]
    #[case::eval(
        "eval",
        serde_json::json!({"eval_id": "ifbench", "dataset_name": "ifbench_v1", "max_tokens": 4096, "temperature": 0.6})
    )]
    #[case::vlm_eval(
        "vlm_eval",
        serde_json::json!({"lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192, "presence_penalty": 1.5})
    )]
    #[case::asr_eval(
        "asr_eval",
        serde_json::json!({"asr_task": "asr_fleurs_nl_nl", "dataset_version": "2026.10.1", "sample_count": 364, "prompt_profile": "voxtral", "max_tokens": 500, "temperature": 0.0})
    )]
    #[case::asr_eval_beams(
        "asr_eval",
        serde_json::json!({"asr_task": "asr_esb_librispeech_test_clean", "dataset_version": "2026.10.1", "sample_count": 2620, "prompt_profile": "minicpm_o", "max_tokens": 128, "num_beams": 3, "beam_repetition_penalty": 1.2})
    )]
    #[case::vl_throughput(
        "vl_throughput",
        serde_json::json!({"image_width": 512, "image_height": 512, "text_tokens": 0, "decode_tokens": 64, "num_images": 1})
    )]
    #[case::vl_max_memory(
        "vl_max_memory",
        serde_json::json!({"image_width": 512, "image_height": 512, "text_tokens": 0, "num_images": 1})
    )]
    #[case::vl_spec_decode(
        "vl_spec_decode",
        serde_json::json!({"draft_length": 8, "max_tokens": 256, "batch_size": 1, "temperature": 0.0})
    )]
    fn declared_benchmark_params_split_into_type_and_untagged_params(
        #[case] benchmark_type: &str,
        #[case] untagged: serde_json::Value,
    ) -> anyhow::Result<()> {
        let declared =
            serde_json::json!({"benchmark_type": benchmark_type, "benchmark_params": untagged});
        let mut payload = sample_payload();
        payload.benchmark_params = BenchmarkParams::from_submission_body(&declared, None)?;
        anyhow::ensure!(payload.benchmark_params.is_some(), "{declared}");
        let json = serde_json::to_value(&payload)?;
        assert_eq!(json["benchmark_type"], benchmark_type);
        assert_eq!(json["benchmark_params"], untagged);
        let round_tripped: BenchmarkSubmissionPayload = serde_json::from_value(json)?;
        assert_eq!(round_tripped, payload);
        Ok(())
    }

    #[test]
    fn undeclared_benchmark_params_omit_both_keys() -> anyhow::Result<()> {
        let json = serde_json::to_value(sample_payload())?;
        assert!(json.get("benchmark_type").is_none(), "{json}");
        assert!(json.get("benchmark_params").is_none(), "{json}");
        Ok(())
    }

    #[rstest]
    #[case::neither(serde_json::json!({}), Ok(None))]
    #[case::both_null(
        serde_json::json!({"benchmark_type": null, "benchmark_params": null}),
        Ok(None)
    )]
    #[case::declared(
        serde_json::json!({"benchmark_type": "max_memory_usage", "benchmark_params": {"prefill_tokens": 256}}),
        Ok(Some(BenchmarkParams::MaxMemoryUsage { prefill_tokens: crate::NonNegativeCount::try_new(256)? }))
    )]
    #[case::inner_tag_agrees(
        serde_json::json!({
            "benchmark_type": "max_memory_usage",
            "benchmark_params": {"benchmark_type": "max_memory_usage", "prefill_tokens": 256},
        }),
        Ok(Some(BenchmarkParams::MaxMemoryUsage { prefill_tokens: crate::NonNegativeCount::try_new(256)? }))
    )]
    #[case::type_alone(
        serde_json::json!({"benchmark_type": "max_memory_usage"}),
        Err("benchmark_type requires benchmark_params")
    )]
    #[case::params_alone(
        serde_json::json!({"benchmark_params": {"prefill_tokens": 256}}),
        Err("benchmark_params requires benchmark_type")
    )]
    #[case::null_type(
        serde_json::json!({"benchmark_type": null, "benchmark_params": {"prefill_tokens": 256}}),
        Err("benchmark_params requires benchmark_type")
    )]
    #[case::unknown_type(
        serde_json::json!({"benchmark_type": "nope", "benchmark_params": {}}),
        Err("invalid benchmark_type")
    )]
    #[case::params_not_an_object(
        serde_json::json!({"benchmark_type": "max_memory_usage", "benchmark_params": 256}),
        Err("expected an object")
    )]
    #[case::inner_tag_disagrees(
        serde_json::json!({
            "benchmark_type": "max_memory_usage",
            "benchmark_params": {"benchmark_type": "prefill_throughput", "prefill_tokens": 256},
        }),
        Err("disagrees with the body's")
    )]
    #[case::nothing_decoded(
        serde_json::json!({
            "benchmark_type": "decode_throughput",
            "benchmark_params": {"prefill_tokens": 256, "decode_tokens": 0, "batch_size": 1},
        }),
        Err("must be greater than 0")
    )]
    #[case::an_image_with_no_width(
        serde_json::json!({
            "benchmark_type": "vl_max_memory",
            "benchmark_params": {"image_width": 0, "image_height": 512},
        }),
        Err("must be greater than 0")
    )]
    #[case::params_of_another_type(
        serde_json::json!({
            "benchmark_type": "decode_throughput",
            "benchmark_params": {"prefill_tokens": 256},
        }),
        Err("missing field `decode_tokens`")
    )]
    fn declared_benchmark_params_from_body(
        #[case] body: serde_json::Value,
        #[case] expected: Result<Option<BenchmarkParams>, &str>,
    ) -> anyhow::Result<()> {
        match (BenchmarkParams::from_submission_body(&body, None), expected) {
            (Ok(got), Ok(want)) => assert_eq!(got, want, "{body}"),
            (Err(e), Err(reason)) => assert!(e.to_string().contains(reason), "{e}"),
            (got, want) => anyhow::bail!("{body}: got {got:?}, want {want:?}"),
        }
        Ok(())
    }

    /// Deserializing the payload refuses what [`BenchmarkParams::from_submission_body`] does.
    #[test]
    fn a_payload_with_a_malformed_declaration_is_refused() -> anyhow::Result<()> {
        let mut json = serde_json::to_value(sample_payload())?;
        json["benchmark_type"] = "max_memory_usage".into();
        let Err(e) = serde_json::from_value::<BenchmarkSubmissionPayload>(json) else {
            anyhow::bail!("accepted a benchmark_type without benchmark_params");
        };
        assert!(
            e.to_string()
                .contains("benchmark_type requires benchmark_params"),
            "{e}"
        );
        Ok(())
    }

    /// Absent, not null, so an unconfigured client needs nothing new from the
    /// receiver.
    #[test]
    fn an_unconfigured_run_omits_the_client_configuration_key() -> anyhow::Result<()> {
        let json = serde_json::to_value(sample_payload())?;
        assert!(json.get("client_configuration").is_none(), "{json}");
        Ok(())
    }

    /// A client that captures nothing, reports no extras and times nothing
    /// sends none of the four keys, so the wire shape is unchanged for it.
    #[test]
    fn an_uncaptured_run_omits_the_output_and_extras_keys() -> anyhow::Result<()> {
        let json = serde_json::to_value(sample_payload())?;
        let present: Vec<_> = [
            "runtime_stdout",
            "runtime_stderr",
            "client_extras",
            "device_time_ms",
        ]
        .into_iter()
        .filter(|key| json.get(key).is_some())
        .collect();
        assert!(present.is_empty(), "{present:?}: {json}");
        Ok(())
    }

    #[rstest]
    #[case::fits("short", 10, "short")]
    #[case::exactly_fits("0123456789", 10, "0123456789")]
    #[case::ascii("0123456789", 7, "…6789")]
    #[case::on_a_char_boundary("ééééé error", 11, "…é error")]
    #[case::inside_a_char("ééééé error", 10, "… error")]
    #[case::budget_below_the_mark("0123456789", 2, "…")]
    fn keep_tail_keeps_the_end_on_a_char_boundary(
        #[case] s: &str,
        #[case] max_bytes: usize,
        #[case] expected: &str,
    ) {
        assert_eq!(keep_tail(s, max_bytes), expected);
    }

    #[rstest]
    #[case::fits_as_sent(r#"[ {"i": 0}, {"i": 1} ]"#, 22, r#"[ {"i": 0}, {"i": 1} ]"#)]
    #[case::drops_the_first(r#"[ {"i": 0}, {"i": 1}, {"i": 2} ]"#, 19, r#"[{"i": 1},{"i": 2}]"#)]
    #[case::keeps_nested_arrays_whole("[[1,2],[3,4]]", 12, "[[3,4]]")]
    #[case::nothing_fits(r#"[{"i":0}]"#, 5, "[]")]
    #[case::last_too_long(r#"[0,{"i":"long"}]"#, 10, "[]")]
    fn keep_last_elements_drops_whole_elements_from_the_front(
        #[case] s: &str,
        #[case] max_bytes: usize,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(keep_last_elements(s, max_bytes)?, expected);
        Ok(())
    }

    #[rstest]
    fn keep_last_elements_refuses_anything_but_a_json_array(
        #[values(r#"{"i":0}"#, r#""[]""#, "throttled=0x0", "", "[1,", r#"…"i":2}]"#)] s: &str,
        #[values(2, MAX_DEVICE_READINGS_BYTES)] max_bytes: usize,
    ) {
        assert!(keep_last_elements(s, max_bytes).is_err(), "{s:?}");
    }

    #[test]
    fn capped_keeps_the_tail_of_each_long_stream() -> anyhow::Result<()> {
        let long = format!("head{}tail", "x".repeat(MAX_RUNTIME_OUTPUT_BYTES));
        let output = RuntimeOutput {
            runtime_stdout: Some(long),
            runtime_stderr: Some("short".to_string()),
        }
        .capped();
        let stdout = output.runtime_stdout.context("stdout kept")?;
        assert_eq!(stdout.len(), MAX_RUNTIME_OUTPUT_BYTES);
        assert!(stdout.starts_with(ELLIPSIS) && stdout.ends_with("xtail"));
        assert_eq!(output.runtime_stderr.as_deref(), Some("short"));
        Ok(())
    }

    #[test]
    fn thermal_headroom_f32_round_trips_under_flatten() -> anyhow::Result<()> {
        // `f32` through `flatten` beside the untagged `BenchmarkResultData`.
        let thermal = ThermalTelemetry::from_series(
            &[
                ThermalReading {
                    android_thermal_headroom: Some(0.31),
                    ..Default::default()
                },
                ThermalReading {
                    android_thermal_headroom: Some(0.44),
                    ..Default::default()
                },
            ],
            &[ThermalReading {
                android_thermal_headroom: Some(0.62),
                ..Default::default()
            }],
        );
        assert_eq!(
            thermal.device_android_thermal_headroom_before,
            Some(vec![0.31, 0.44])
        );
        let mut payload = sample_payload();
        payload.thermal = thermal.clone();
        let json = serde_json::to_value(&payload)?;
        assert!(json["device_android_thermal_headroom_before"][0].is_number());
        let round_tripped: BenchmarkSubmissionPayload = serde_json::from_value(json)?;
        assert_eq!(round_tripped.thermal, thermal);
        Ok(())
    }

    #[test]
    fn apple_soc_temp_c_f32_round_trips_under_flatten() -> anyhow::Result<()> {
        let thermal = ThermalTelemetry::from_series(
            &[
                ThermalReading {
                    apple_soc_temp_c: Some(38.5),
                    ..Default::default()
                },
                ThermalReading {
                    apple_soc_temp_c: Some(41.25),
                    ..Default::default()
                },
            ],
            &[ThermalReading {
                apple_soc_temp_c: Some(44.0),
                ..Default::default()
            }],
        );
        assert_eq!(
            thermal.device_apple_soc_temp_c_before,
            Some(vec![38.5, 41.25])
        );
        assert_eq!(thermal.device_apple_soc_temp_c_after, Some(vec![44.0]));
        let mut payload = sample_payload();
        payload.thermal = thermal.clone();
        let json = serde_json::to_value(&payload)?;
        assert!(json["device_apple_soc_temp_c_before"][0].is_number());
        let round_tripped: BenchmarkSubmissionPayload = serde_json::from_value(json)?;
        assert_eq!(round_tripped.thermal, thermal);
        Ok(())
    }
    #[test]
    fn submission_payload_carries_the_descriptors() -> anyhow::Result<()> {
        let value = serde_json::to_value(sample_payload())?;
        assert_eq!(value["model_descriptor"], MODEL_DESC);
        assert_eq!(value["runtime_descriptor"], RT_DESC);
        Ok(())
    }

    // mgmt-side ingestion / dashboards key on the literal string here, so the
    // wire field must carry exactly what `canonical_string()` emits (its
    // per-flag spelling is pinned in `cell::model_flags`).
    #[rstest::rstest]
    #[case::unset(None)]
    #[case::thinking_on(Some(true))]
    fn submission_payload_model_flags_wire_shape(
        #[case] enable_thinking: Option<bool>,
    ) -> anyhow::Result<()> {
        let flags = ModelFlags::EvalGgufText {
            enable_thinking,
            reasoning_effort: None,
        };
        let mut payload = sample_payload();
        payload.model_flags = flags.canonical_string();
        let json = serde_json::to_value(&payload)?;
        let actual = json.get("model_flags").and_then(serde_json::Value::as_str);
        assert_eq!(actual, flags.canonical_string().as_deref());
        Ok(())
    }
}
