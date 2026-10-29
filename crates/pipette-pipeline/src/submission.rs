//! The server's side of a device's result: validating the wire
//! [`SubmissionInput`], and the stored [`Submission`] it becomes, which
//! flattens the input and adds the server-controlled identity fields.

use std::collections::HashMap;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pipette_core::benchmark;
use pipette_core::benchmark::Benchmark;
use pipette_core::warehouse;
use pipette_plan_types::{
    AsrAudioTokensSource, AsrClientExtras, AsrClientExtrasError, AsrContextSizing,
    AsrThroughputBenchmark, BenchmarkDefinition, BenchmarkId, BenchmarkParams, BenchmarkType,
    ClientId, Dataset, DeviceFormFactor, FailureInput, FailureSource, FleetDetail, JobId,
    MAX_CLIENT_EXTRAS_BYTES, MAX_DEVICE_READINGS_BYTES, MAX_READINESS_GATE_WAITS_BYTES,
    NonEmptyTrimmedString, NonNegativeMillis, PositiveCount, PositiveMillis, RuntimeOutput,
    SampleCompletion, Sha256, SubmissionInput, SuccessInput, VlSpecDecodeBenchmark,
    keep_last_elements,
};
use pipette_store::{CreateOutcome, SubmissionStore};

/// The server's half of a [`SubmissionInput`]: its checks against the catalog
/// entry, and the record it becomes.
pub trait SubmissionInputExt {
    fn validate(&self, benchmark: &Benchmark) -> Result<(), ValidationError>;

    fn into_submission(
        self,
        client_id: ClientId,
        job_id: JobId,
        submitted_at: DateTime<Utc>,
        benchmark_type: BenchmarkType,
    ) -> Submission;
}

impl SubmissionInputExt for SubmissionInput {
    fn validate(&self, benchmark: &Benchmark) -> Result<(), ValidationError> {
        match self {
            Self::Success(s) => s.validate(benchmark),
            Self::Failure(f) => {
                validate_json_ref("model_descriptor", f.model_descriptor.as_deref())?;
                validate_json_ref("runtime_descriptor", f.runtime_descriptor.as_deref())?;
                validate_client_extras(f.client_extras.as_deref())
            }
        }
    }

    /// Cuts output and telemetry to their caps before anything stores them; the
    /// handler stamps `requested_*_flags` afterwards from the claimed job.
    fn into_submission(
        self,
        client_id: ClientId,
        job_id: JobId,
        submitted_at: DateTime<Utc>,
        benchmark_type: BenchmarkType,
    ) -> Submission {
        match self {
            Self::Success(mut wire) => {
                fit_device_telemetry(&mut wire, &client_id, &job_id);
                Submission::Success(Box::new(SuccessSubmission {
                    wire: SuccessInput {
                        runtime_output: std::mem::take(&mut wire.runtime_output).capped(),
                        ..*wire
                    },
                    client_id,
                    job_id,
                    submitted_at,
                    benchmark_type,
                    requested_model_flags: None,
                    requested_runtime_flags: None,
                    requested_benchmark_flags: None,
                    rescored_from: None,
                    run_submitted_at: None,
                }))
            }
            Self::Failure(mut wire) => Submission::Failure(Box::new(FailureSubmission {
                wire: FailureInput {
                    runtime_output: std::mem::take(&mut wire.runtime_output).capped(),
                    ..*wire
                },
                client_id,
                job_id,
                submitted_at,
                benchmark_type,
                requested_model_flags: None,
                requested_runtime_flags: None,
                requested_benchmark_flags: None,
                failure_source: Some(FailureSource::Client),
            })),
        }
    }
}

/// A run's `vl_spec_decode` counters, once the required ones are known present.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VlSpecDecodeCounters {
    pub turns: i32,
    pub prompt_ms: f32,
    pub predicted_ms: f32,
    pub ttft_ms: f32,
    pub decode_ms: f32,
    pub e2e_ms: f32,
    pub generated_tokens: i64,
    /// `Some` exactly when speculation was on.
    pub drafter: Option<DrafterCounters>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DrafterCounters {
    pub drafted_tokens: i64,
    pub accepted_tokens: i64,
    pub turn_accept_length_sum: f32,
}

/// A run's `asr_throughput` measurements, once all four required ones are
/// known present and the audio is known to be part of the prompt. Validation
/// and scoring read the wire through this one extraction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsrThroughputMeasurements {
    pub prompt_tokens: PositiveCount,
    pub audio_tokens: PositiveCount,
    pub prompt_ms: PositiveMillis,
    pub prompt_ms_stddev: Option<NonNegativeMillis>,
    pub predicted_ms: PositiveMillis,
    pub predicted_ms_stddev: Option<NonNegativeMillis>,
    /// The request's span on the client's clock (`total_time_ms`), as
    /// `vl_throughput` takes it. `None` from a client that does not time it.
    pub total_time_ms: Option<PositiveMillis>,
    pub total_time_ms_stddev: Option<NonNegativeMillis>,
}

impl AsrThroughputMeasurements {
    /// The audio and the entry's `text_tokens` are part of the prompt, never
    /// all of it: the prompt holds the template's tokens too. Checked against
    /// the entry at intake, and with no text wherever the entry is not at hand,
    /// which no row that passes intake can fail.
    pub fn within_prompt(&self, text_tokens: i32) -> Result<(), ValidationError> {
        let audio_and_text = i64::from(i32::from(self.audio_tokens)) + i64::from(text_tokens);
        if audio_and_text >= i64::from(i32::from(self.prompt_tokens)) {
            return Err(ValidationError::AudioTokensNotWithinPrompt {
                audio_tokens: i32::from(self.audio_tokens),
                text_tokens,
                prompt_tokens: i32::from(self.prompt_tokens),
            });
        }
        Ok(())
    }
}

/// The rules a success report is held to beyond its shape.
pub trait SuccessInputExt {
    fn validate(&self, benchmark: &Benchmark) -> Result<(), ValidationError>;

    fn speculative_enabled(&self) -> Result<bool, ValidationError>;

    fn vl_spec_decode_counters(&self) -> Result<VlSpecDecodeCounters, ValidationError>;

    fn asr_throughput_measurements(&self) -> Result<AsrThroughputMeasurements, ValidationError>;

    fn validate_vl_spec_decode(&self, draft_length: i32) -> Result<(), ValidationError>;
}

impl SuccessInputExt for SuccessInput {
    /// The rules serde can't express on the shape; each number's own range is
    /// held by its type.
    fn validate(&self, benchmark: &Benchmark) -> Result<(), ValidationError> {
        self.device_form_factor
            .parse::<DeviceFormFactor>()
            .map_err(|e| ValidationError::FormFactor(e.to_string()))?;

        if let (Some(t), Some(a)) = (
            self.model_params_total_millions,
            self.model_params_active_millions,
        ) && a > t
        {
            return Err(ValidationError::ActiveExceedsTotal {
                active: a.into(),
                total: t.into(),
            });
        }

        if self.device_gpu_vram_bytes.is_some() && self.device_gpu_model.is_none() {
            return Err(ValidationError::GpuVramRequiresGpuModel);
        }
        if self.device_npu_vram_bytes.is_some() && self.device_npu_model.is_none() {
            return Err(ValidationError::NpuVramRequiresNpuModel);
        }

        // Before the ASR arms read it: an oversized value is never parsed, and
        // a non-object is named as one, not as a missing ASR key.
        validate_client_extras(self.client_extras.as_deref())?;
        match &benchmark.def {
            BenchmarkDefinition::PrefillThroughput(_) => {
                if self.prefill_time_ms.is_none() {
                    return Err(ValidationError::MissingMetric("prefill_time_ms"));
                }
            }
            BenchmarkDefinition::DecodeThroughput(_) => {
                if self.decode_time_ms.is_none() {
                    return Err(ValidationError::MissingMetric("decode_time_ms"));
                }
            }
            BenchmarkDefinition::EndToEndLatency(_) => {
                if self.total_time_ms.is_none() {
                    return Err(ValidationError::MissingMetric("total_time_ms"));
                }
            }
            BenchmarkDefinition::MaxMemoryUsage(_) | BenchmarkDefinition::VlMaxMemory(_) => {
                if self.max_host_bytes.is_none() {
                    return Err(ValidationError::MissingMetric("max_host_bytes"));
                }
            }
            BenchmarkDefinition::AsrMaxMemory(_) => {
                if self.max_host_bytes.is_none() {
                    return Err(ValidationError::MissingMetric("max_host_bytes"));
                }
                let extras = AsrClientExtras::parse(self.client_extras.as_deref())?;
                asr_context_sizing(&extras)?;
                asr_beyond_model_audio_limit(&extras)?;
            }
            BenchmarkDefinition::Eval(_)
            | BenchmarkDefinition::VlmEval(_)
            | BenchmarkDefinition::AsrEval(_) => {
                let completions = self
                    .completions
                    .as_deref()
                    .filter(|c| !c.is_empty())
                    .ok_or(ValidationError::MissingMetric("completions"))?;
                // Refused here, not at scoring: the client then keeps its
                // checkpoint, and nothing is queued that can never be scored.
                answers_every_sample(&benchmark.def, completions)?;
                if let Some(dataset) = benchmark.dataset() {
                    reports_the_pinned_corpus(self.observation_dataset_digest.as_ref(), dataset)?;
                }
                // The scoring service rejects these too, but only at cron
                // time, leaving the body parked in `incoming/`.
                let mut seen =
                    std::collections::HashMap::<&str, usize>::with_capacity(completions.len());
                for (i, c) in completions.iter().enumerate() {
                    if let Some(prev) = seen.insert(c.id.as_str(), i) {
                        return Err(ValidationError::DuplicateCompletionId {
                            index: i,
                            id: c.id.clone().into(),
                            first_seen: prev,
                        });
                    }
                }
            }
            BenchmarkDefinition::VlThroughput(_) => {
                if self.prompt_tokens.is_none() {
                    return Err(ValidationError::MissingMetric("prompt_tokens"));
                }
                if self.prompt_ms.is_none() {
                    return Err(ValidationError::MissingMetric("prompt_ms"));
                }
                if self.predicted_ms.is_none() {
                    return Err(ValidationError::MissingMetric("predicted_ms"));
                }
            }
            BenchmarkDefinition::AsrThroughput(AsrThroughputBenchmark {
                parameter_text_tokens,
                ..
            }) => {
                self.asr_throughput_measurements()?
                    .within_prompt(parameter_text_tokens.into_inner())?;
                let extras = AsrClientExtras::parse(self.client_extras.as_deref())?;
                asr_regime(
                    asr_context_sizing(&extras)?,
                    asr_audio_tokens_source(&extras)?,
                )?;
                asr_beyond_model_audio_limit(&extras)?;
            }
            BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
                parameter_draft_length,
                ..
            }) => self.validate_vl_spec_decode(parameter_draft_length.into_inner())?,
        }

        validate_json_ref("model_descriptor", self.model_descriptor.as_deref())?;
        validate_json_ref("runtime_descriptor", self.runtime_descriptor.as_deref())?;
        validate_json_object("benchmark_flags", self.benchmark_flags.as_deref())?;
        validate_json_object("client_configuration", self.client_configuration.as_deref())
    }

    /// Required on a corpus run: the row's mode is what its metrics mean.
    fn speculative_enabled(&self) -> Result<bool, ValidationError> {
        speculative_enabled(self.runtime_flags.as_ref().map(AsRef::as_ref)).ok_or(
            ValidationError::MissingMetric("runtime_flags.speculative_enabled"),
        )
    }

    fn asr_throughput_measurements(&self) -> Result<AsrThroughputMeasurements, ValidationError> {
        let measurements = AsrThroughputMeasurements {
            prompt_tokens: required(self.asr_prompt_tokens, "asr_prompt_tokens")?,
            audio_tokens: required(self.asr_audio_tokens, "asr_audio_tokens")?,
            prompt_ms: required(self.asr_prompt_ms, "asr_prompt_ms")?,
            prompt_ms_stddev: self.asr_prompt_ms_stddev,
            predicted_ms: required(self.asr_predicted_ms, "asr_predicted_ms")?,
            predicted_ms_stddev: self.asr_predicted_ms_stddev,
            total_time_ms: self.total_time_ms,
            total_time_ms_stddev: self.total_time_ms_stddev,
        };
        measurements.within_prompt(0)?;
        Ok(measurements)
    }

    /// The drafter's three counters are all present or all absent.
    fn vl_spec_decode_counters(&self) -> Result<VlSpecDecodeCounters, ValidationError> {
        let drafter = match (
            self.vl_spec_decode_drafted_tokens,
            self.vl_spec_decode_accepted_tokens,
            self.vl_spec_decode_turn_accept_length_sum,
        ) {
            (Some(drafted_tokens), Some(accepted_tokens), Some(turn_accept_length_sum)) => {
                Some(DrafterCounters {
                    drafted_tokens,
                    accepted_tokens,
                    turn_accept_length_sum,
                })
            }
            (None, None, None) => None,
            (drafted, accepted, _) => {
                let field = if drafted.is_none() {
                    "drafted_tokens"
                } else if accepted.is_none() {
                    "accepted_tokens"
                } else {
                    "turn_accept_length_sum"
                };
                return Err(out_of_range(
                    format!("vl_spec_decode_{field}"),
                    "is missing: the three drafter counters come together or not at all",
                ));
            }
        };
        Ok(VlSpecDecodeCounters {
            turns: required(self.vl_spec_decode_turns, "vl_spec_decode_turns")?.into(),
            prompt_ms: required(self.vl_spec_decode_prompt_ms, "vl_spec_decode_prompt_ms")?.into(),
            predicted_ms: required(
                self.vl_spec_decode_predicted_ms,
                "vl_spec_decode_predicted_ms",
            )?
            .into(),
            ttft_ms: required(self.vl_spec_decode_ttft_ms, "vl_spec_decode_ttft_ms")?.into(),
            decode_ms: required(self.vl_spec_decode_decode_ms, "vl_spec_decode_decode_ms")?.into(),
            e2e_ms: required(self.vl_spec_decode_e2e_ms, "vl_spec_decode_e2e_ms")?.into(),
            generated_tokens: required(
                self.vl_spec_decode_generated_tokens,
                "vl_spec_decode_generated_tokens",
            )?,
            drafter,
        })
    }

    fn validate_vl_spec_decode(&self, draft_length: i32) -> Result<(), ValidationError> {
        let counters = self.vl_spec_decode_counters()?;
        match (self.speculative_enabled()?, counters.drafter.is_some()) {
            (true, false) => Err(out_of_range(
                "vl_spec_decode_drafted_tokens".to_string(),
                "is required when runtime_flags.speculative_enabled is true",
            )),
            (false, true) => Err(out_of_range(
                "vl_spec_decode_drafted_tokens".to_string(),
                "must be absent when runtime_flags.speculative_enabled is false",
            )),
            _ => counters.validate(draft_length),
        }
    }
}

pub fn speculative_enabled(runtime_flags: Option<&str>) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(runtime_flags?)
        .ok()?
        .get("speculative_enabled")?
        .as_bool()
}

impl VlSpecDecodeCounters {
    /// Positive denominators and bounded ratios, else `inf` or a ratio above
    /// 1.0 lands as a number. Refusals, never clamps: a clamp invents a result.
    fn validate(&self, draft_length: i32) -> Result<(), ValidationError> {
        let f = |name| format!("vl_spec_decode_{name}");
        positive_count(f("generated_tokens"), self.generated_tokens)?;
        at_least_f32(
            f("e2e_ms"),
            self.e2e_ms,
            self.ttft_ms + self.decode_ms,
            "must not be less than ttft_ms + decode_ms",
        )?;
        let Some(d) = &self.drafter else {
            return Ok(());
        };
        positive_count(f("drafted_tokens"), d.drafted_tokens)?;
        non_negative_count(f("accepted_tokens"), d.accepted_tokens)?;
        at_most(
            f("accepted_tokens"),
            d.accepted_tokens,
            d.drafted_tokens,
            "must not exceed drafted_tokens",
        )?;
        // Keeps the legacy `accept_length` division defined; per turn,
        // accepted can exceed generated, so this is a traversal-wide bound.
        less_than(
            f("accepted_tokens"),
            d.accepted_tokens,
            self.generated_tokens,
            "must be less than generated_tokens",
        )?;
        // A turn reads 0 (one-token reply) to `k + 1` (a forward commits at
        // most `k` drafts plus the target's token). `>= 0.0` also rejects NaN.
        let sum = d.turn_accept_length_sum;
        let max = self.turns as f32 * (draft_length as f32 + 1.0);
        (sum >= 0.0 && sum <= max).then_some(()).ok_or_else(|| {
            out_of_range(
                f("turn_accept_length_sum"),
                "must be between 0 and turns * (parameter_draft_length + 1)",
            )
        })
    }
}

/// How an ASR row's context was sized, required: only an `exact` peak or
/// prefill is the clip's own, so a row that does not say cannot be compared.
fn asr_context_sizing(extras: &AsrClientExtras) -> Result<AsrContextSizing, ValidationError> {
    required(extras.context_sizing, "client_extras.asr_context_sizing")
}

fn asr_audio_tokens_source(
    extras: &AsrClientExtras,
) -> Result<AsrAudioTokensSource, ValidationError> {
    required(
        extras.audio_tokens_source,
        "client_extras.asr_audio_tokens_source",
    )
}

/// Whether an ASR row's clip is past the model's documented audio limit,
/// required: a row that does not say could be a past-the-limit speed
/// scenario shown as a supported one.
fn asr_beyond_model_audio_limit(extras: &AsrClientExtras) -> Result<bool, ValidationError> {
    required(
        extras.beyond_model_audio_limit,
        "client_extras.asr_beyond_model_audio_limit",
    )
}

fn required<T>(value: Option<T>, field: &'static str) -> Result<T, ValidationError> {
    value.ok_or(ValidationError::MissingMetric(field))
}

fn out_of_range(field: String, detail: &'static str) -> ValidationError {
    ValidationError::VlSpecDecodeOutOfRange { field, detail }
}

fn positive_count(field: String, value: i64) -> Result<(), ValidationError> {
    (value > 0)
        .then_some(())
        .ok_or_else(|| out_of_range(field, "must be greater than zero"))
}

fn non_negative_count(field: String, value: i64) -> Result<(), ValidationError> {
    (value >= 0)
        .then_some(())
        .ok_or_else(|| out_of_range(field, "must not be negative"))
}

fn at_most(
    field: String,
    value: i64,
    bound: i64,
    detail: &'static str,
) -> Result<(), ValidationError> {
    (value <= bound)
        .then_some(())
        .ok_or_else(|| out_of_range(field, detail))
}

/// The strict form of [`at_most`], for a counter whose distance from its bound
/// is a denominator: equality there is a division by zero, not a tight run.
fn less_than(
    field: String,
    value: i64,
    bound: i64,
    detail: &'static str,
) -> Result<(), ValidationError> {
    (value < bound)
        .then_some(())
        .ok_or_else(|| out_of_range(field, detail))
}

/// A total against the phases it contains. The slack absorbs `f32` rounding
/// over hundreds of summed turns.
fn at_least_f32(
    field: String,
    value: f32,
    bound: f32,
    detail: &'static str,
) -> Result<(), ValidationError> {
    (value >= bound - bound * 1e-3)
        .then_some(())
        .ok_or_else(|| out_of_range(field, detail))
}

/// A descriptor is opaque; this only checks it parses as JSON, so the scorer
/// can canonicalize it.
fn validate_json_ref(field: &'static str, value: Option<&str>) -> Result<(), ValidationError> {
    if let Some(s) = value {
        serde_json::from_str::<serde_json::Value>(s).map_err(|e| {
            ValidationError::InvalidJsonRef {
                field,
                detail: e.to_string(),
            }
        })?;
    }
    Ok(())
}

/// The only object check before the warehouse: `canonicalize_str` passes
/// unparseable input through unchanged.
fn validate_json_object(field: &'static str, value: Option<&str>) -> Result<(), ValidationError> {
    let Some(s) = value else { return Ok(()) };
    let parsed = serde_json::from_str::<serde_json::Value>(s).map_err(|e| {
        ValidationError::InvalidJsonRef {
            field,
            detail: e.to_string(),
        }
    })?;
    if !parsed.is_object() {
        return Err(ValidationError::InvalidJsonRef {
            field,
            detail: "expected a JSON object".to_string(),
        });
    }
    Ok(())
}

/// The size is checked first, so an oversized value is never parsed.
fn validate_client_extras(value: Option<&str>) -> Result<(), ValidationError> {
    if value.is_some_and(|s| s.len() > MAX_CLIENT_EXTRAS_BYTES) {
        return Err(ValidationError::TooLarge {
            field: "client_extras",
            limit: MAX_CLIENT_EXTRAS_BYTES,
        });
    }
    validate_json_object("client_extras", value)
}

/// Cut each device telemetry field to its cap and drop a non-array, never
/// refusing: a `400` would cost the measurement over a caveat on it.
fn fit_device_telemetry(wire: &mut SuccessInput, client_id: &ClientId, job_id: &JobId) {
    let fields = [
        (
            "device_hardware_readings",
            &mut wire.device_hardware_readings,
            MAX_DEVICE_READINGS_BYTES,
        ),
        (
            "device_os_throttling_readings",
            &mut wire.device_os_throttling_readings,
            MAX_DEVICE_READINGS_BYTES,
        ),
        (
            "readiness_gate_waits",
            &mut wire.readiness_gate_waits,
            MAX_READINESS_GATE_WAITS_BYTES,
        ),
    ];
    for (field, slot, cap) in fields {
        let Some(sent) = slot.take() else { continue };
        *slot = match keep_last_elements(&sent, cap) {
            Ok(kept) => {
                if sent.len() > cap {
                    tracing::warn!(
                        job_id = %job_id,
                        client_id = %client_id,
                        field,
                        sent_bytes = sent.len(),
                        kept_bytes = kept.len(),
                        "device telemetry over its cap: kept its last elements"
                    );
                }
                Some(kept)
            }
            Err(e) => {
                tracing::warn!(
                    job_id = %job_id,
                    client_id = %client_id,
                    field,
                    error = %e,
                    "device telemetry is not a JSON array: dropped"
                );
                None
            }
        };
    }
}

/// The submission body stored under `submissions/`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
pub enum Submission {
    Success(Box<SuccessSubmission>),
    Failure(Box<FailureSubmission>),
}

impl Submission {
    pub fn job_id(&self) -> &JobId {
        match self {
            Self::Success(s) => &s.job_id,
            Self::Failure(f) => &f.job_id,
        }
    }

    /// Server-side only, from the lease the server issued, so a reader can
    /// trust it: the `*Input` types carry no such field for a client to fill.
    pub fn set_requested_flags(&mut self, flags: RequestedFlags) {
        let (model, runtime, benchmark) = match self {
            Self::Success(s) => (
                &mut s.requested_model_flags,
                &mut s.requested_runtime_flags,
                &mut s.requested_benchmark_flags,
            ),
            Self::Failure(f) => (
                &mut f.requested_model_flags,
                &mut f.requested_runtime_flags,
                &mut f.requested_benchmark_flags,
            ),
        };
        *model = flags.model;
        *runtime = flags.runtime;
        *benchmark = flags.benchmark;
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SuccessSubmission {
    #[serde(flatten)]
    pub wire: SuccessInput,
    pub client_id: ClientId,
    pub job_id: JobId,
    pub submitted_at: DateTime<Utc>,
    pub benchmark_type: BenchmarkType,
    /// Canonical JSON with every `auth_token` dropped.
    #[serde(default)]
    pub requested_model_flags: Option<String>,
    #[serde(default)]
    pub requested_runtime_flags: Option<String>,
    #[serde(default)]
    pub requested_benchmark_flags: Option<String>,
    /// The job whose record this re-scored copy copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescored_from: Option<JobId>,
    /// When a device submitted the run whose completions this copy scores
    /// again. The server sets it together with [`Self::rescored_from`]. See
    /// `crate::recalculation::run_submitted_at`. `None` on every body a client
    /// sent, and on a copy whose chain of copies reaches a record that is gone
    /// or is not a success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FailureSubmission {
    #[serde(flatten)]
    pub wire: FailureInput,
    pub client_id: ClientId,
    pub job_id: JobId,
    pub submitted_at: DateTime<Utc>,
    pub benchmark_type: BenchmarkType,
    #[serde(default)]
    pub requested_model_flags: Option<String>,
    #[serde(default)]
    pub requested_runtime_flags: Option<String>,
    #[serde(default)]
    pub requested_benchmark_flags: Option<String>,
    /// `None` on an older body; the scorer then resolves it from `client_id`.
    #[serde(default)]
    pub failure_source: Option<FailureSource>,
}

/// A vlm_eval or an asr_eval is a result only with one answer to each of its
/// samples: its headline is defined over the whole dataset, and a row with no
/// answer scored as a wrong one (or as an empty transcript) would make the
/// number a property of the run. A row is no answer when it failed
/// (`stop_reason: "failure"`, or the legacy `failed` flag, so a row whose two
/// fields disagree is refused too); any reply the model gave, empty or cut
/// off, is one. Read on the rows as sent, before anything collapses a
/// repeated id. Every other type has no such rule and passes.
pub fn answers_every_sample(
    def: &BenchmarkDefinition,
    completions: &[SampleCompletion],
) -> Result<(), ValidationError> {
    let (eval, sample_count) = match def {
        BenchmarkDefinition::VlmEval(vlm) => ("a vlm_eval", vlm.parameter_sample_count),
        BenchmarkDefinition::AsrEval(asr) => ("an asr_eval", asr.parameter_sample_count),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::Eval(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_) => return Ok(()),
    };
    let samples = usize::try_from(sample_count.into_inner()).unwrap_or(usize::MAX);
    let failed = completions
        .iter()
        .filter(|c| c.failed || c.stop_reason.as_deref() == Some("failure"))
        .count();
    let distinct = completions
        .iter()
        .map(|c| c.id.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    if completions.len() == samples && distinct == samples && failed == 0 {
        Ok(())
    } else {
        Err(ValidationError::IncompleteEval {
            eval,
            samples,
            rows: completions.len(),
            distinct,
            failed,
        })
    }
}

/// A corpus eval's answers are to the prompts of the corpus it ran, so they
/// grade only if that is the corpus its entry pins.
pub fn reports_the_pinned_corpus(
    reported: Option<&Sha256>,
    dataset: &Dataset,
) -> Result<(), ValidationError> {
    if reported == Some(&dataset.digest) {
        return Ok(());
    }
    Err(ValidationError::UnpinnedCorpus {
        reported: reported.map_or_else(|| "(none reported)".to_owned(), ToString::to_string),
        pinned: dataset.digest.clone(),
    })
}

/// Domain-level submission validation failures, independent of HTTP.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("{0}")]
    FormFactor(String),
    #[error(
        "model_params_active_millions ({active}) must not exceed \
         model_params_total_millions ({total})"
    )]
    ActiveExceedsTotal { active: i32, total: i32 },
    #[error("device_gpu_vram_bytes requires device_gpu_model")]
    GpuVramRequiresGpuModel,
    #[error("device_npu_vram_bytes requires device_npu_model")]
    NpuVramRequiresNpuModel,
    #[error("missing {0}")]
    MissingMetric(&'static str),
    #[error("{field} is present but not valid JSON: {detail}")]
    InvalidJsonRef { field: &'static str, detail: String },
    #[error("{field} is over its {limit}-byte limit")]
    TooLarge { field: &'static str, limit: usize },
    #[error(
        "duplicate completion id at completions[{index}]: {id:?} \
         (first seen at completions[{first_seen}])"
    )]
    DuplicateCompletionId {
        index: usize,
        id: String,
        first_seen: usize,
    },
    /// `eval` is the family with its article: "a vlm_eval", "an asr_eval".
    #[error(
        "{eval} is a result only with one answer to each of its {samples} samples: \
         {rows} rows, {distinct} distinct ids, {failed} failed"
    )]
    IncompleteEval {
        eval: &'static str,
        samples: usize,
        rows: usize,
        distinct: usize,
        failed: usize,
    },
    #[error(
        "observation_dataset_digest {reported} is not {pinned}, the corpus the entry pins: \
         its answers are not to the prompts the scorer grades"
    )]
    UnpinnedCorpus { reported: String, pinned: Sha256 },
    #[error("{new} and legacy {legacy} both present; send only one")]
    AliasCollision {
        new: &'static str,
        legacy: &'static str,
    },
    #[error("failure submission must not carry device or measurement fields: {fields}")]
    FailureCarriesMeasurement { fields: String },
    #[error("success submission must not carry failure-only fields: {fields}")]
    SuccessCarriesFailureFields { fields: String },
    #[error("{field} {detail}")]
    VlSpecDecodeOutOfRange { field: String, detail: &'static str },
    #[error(
        "asr_audio_tokens ({audio_tokens}) and the entry's {text_tokens} text tokens must be \
         less than asr_prompt_tokens ({prompt_tokens}), which holds the template's tokens too"
    )]
    AudioTokensNotWithinPrompt {
        audio_tokens: i32,
        text_tokens: i32,
        prompt_tokens: i32,
    },
    #[error(
        "client_extras: asr_context_sizing {sizing} with asr_audio_tokens_source {audio_tokens_source} is \
         not a pair a client produces: an exact context is sized from the calibrated count, \
         and an upper bound is used only when there is none"
    )]
    AsrRegimeMismatch {
        sizing: AsrContextSizing,
        audio_tokens_source: AsrAudioTokensSource,
    },
    #[error(transparent)]
    AsrClientExtras(#[from] AsrClientExtrasError),
    #[error("benchmark_type {declared} does not match {resolved}, the type of the benchmark")]
    BenchmarkTypeMismatch {
        declared: BenchmarkType,
        resolved: BenchmarkType,
    },
    /// Names only the catalog's side; the client knows what it sent.
    #[error("benchmark_params do not match {0}, the benchmark's")]
    BenchmarkParamsMismatch(String),
}

/// The pairs a client produces: `exact` only from the calibrated count it was
/// sized from, `upper_bound` only when there was none, so the count is
/// measured. A pinned context was sized by the plan, so either count can sit
/// behind it.
fn asr_regime(
    sizing: AsrContextSizing,
    source: AsrAudioTokensSource,
) -> Result<(), ValidationError> {
    match (sizing, source) {
        (AsrContextSizing::Exact, AsrAudioTokensSource::Calibrated)
        | (AsrContextSizing::UpperBound, AsrAudioTokensSource::Measured)
        | (AsrContextSizing::Pinned, _) => Ok(()),
        (AsrContextSizing::Exact, AsrAudioTokensSource::Measured)
        | (AsrContextSizing::UpperBound, AsrAudioTokensSource::Calibrated) => {
            Err(ValidationError::AsrRegimeMismatch {
                sizing,
                audio_tokens_source: source,
            })
        }
    }
}

const FAILURE_ONLY_FIELDS: &[&str] = &["retriable", "failure_reason"];

/// Not `device_time_ms`: a failed job also takes time.
fn is_success_only_field(key: &str, measurements: &std::collections::HashSet<&str>) -> bool {
    (key.starts_with("device_") && key != "device_time_ms") || measurements.contains(key)
}

/// Refuse a body that mixes the two `message_type` shapes.
///
/// Unknown keys are still ignored, but a known field of the other variant is a
/// client bug that dropping would hide. On the raw body: serde discards these.
pub fn reject_wrong_variant_fields(
    body: &serde_json::Value,
    is_failure: bool,
) -> Result<(), ValidationError> {
    let Some(obj) = body.as_object() else {
        return Ok(());
    };
    let mut offenders: Vec<&str> = if is_failure {
        let measurements: std::collections::HashSet<&str> =
            benchmark::all_measurement_fields().collect();
        obj.keys()
            .map(String::as_str)
            .filter(|k| is_success_only_field(k, &measurements))
            .collect()
    } else {
        FAILURE_ONLY_FIELDS
            .iter()
            .filter(|f| obj.contains_key(**f))
            .copied()
            .collect()
    };
    if offenders.is_empty() {
        return Ok(());
    }
    // Sorted so one client's bug reads identically in every log line.
    offenders.sort_unstable();
    let fields = offenders.join(", ");
    Err(if is_failure {
        ValidationError::FailureCarriesMeasurement { fields }
    } else {
        ValidationError::SuccessCarriesFailureFields { fields }
    })
}

/// On the raw body: `#[serde(alias)]` silently keeps one side on collision.
pub fn reject_max_alias_collisions(body: &serde_json::Value) -> Result<(), ValidationError> {
    for (new, legacy) in [
        ("max_host_bytes", "max_ram_bytes"),
        ("max_gpu_bytes", "max_vram_bytes"),
    ] {
        if body.get(new).is_some() && body.get(legacy).is_some() {
            return Err(ValidationError::AliasCollision { new, legacy });
        }
    }
    Ok(())
}

/// A disagreeing declaration means the client ran something else; a knob the
/// catalog leaves unpinned is the client's to pick. `declared` must be read
/// over `catalog` (`BenchmarkParams::from_submission_body`), or omitted fields
/// read as mismatches.
/// An eval's `max_context` is no knob: it must agree, as `max_tokens` does.
/// Nor is anything a vlm_eval leaves unpinned the client's: that is greedy with
/// every other sampler neutral, so each of its knobs must agree too.
pub fn check_declared_benchmark_params(
    declared: &BenchmarkParams,
    catalog: &BenchmarkParams,
) -> Result<(), ValidationError> {
    if declared.benchmark_type() != catalog.benchmark_type() {
        return Err(ValidationError::BenchmarkTypeMismatch {
            declared: declared.benchmark_type(),
            resolved: catalog.benchmark_type(),
        });
    }
    if without_unpinned_knobs(declared.clone(), catalog) != *catalog {
        return Err(ValidationError::BenchmarkParamsMismatch(
            serde_json::to_string(catalog).unwrap_or_default(),
        ));
    }
    Ok(())
}

fn without_unpinned_knobs(
    mut declared: BenchmarkParams,
    catalog: &BenchmarkParams,
) -> BenchmarkParams {
    if let (
        BenchmarkParams::Eval {
            mcq_choices,
            top_p,
            top_k,
            min_p,
            seed,
            ..
        },
        BenchmarkParams::Eval {
            mcq_choices: pinned_mcq_choices,
            top_p: pinned_top_p,
            top_k: pinned_top_k,
            min_p: pinned_min_p,
            seed: pinned_seed,
            ..
        },
    ) = (&mut declared, catalog)
    {
        if pinned_mcq_choices.is_none() {
            *mcq_choices = None;
        }
        if pinned_top_p.is_none() {
            *top_p = None;
        }
        if pinned_top_k.is_none() {
            *top_k = None;
        }
        if pinned_min_p.is_none() {
            *min_p = None;
        }
        if pinned_seed.is_none() {
            *seed = None;
        }
    }
    declared
}

/// The flag groups a job's `spec` asks for, as canonical JSON with every
/// `auth_token` dropped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RequestedFlags {
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub benchmark: Option<String>,
}

/// Stored as authored, discriminants included, so the requested form is
/// expected to differ from what a client reports.
pub fn requested_flags_from_spec(spec: &serde_json::Value) -> RequestedFlags {
    RequestedFlags {
        model: requested_flag_group(spec, "model_flags"),
        runtime: requested_flag_group(spec, "runtime_flags"),
        benchmark: requested_flag_group(spec, "benchmark_flags"),
    }
}

/// Absent, `null` and a top-level `{}` all mean "no request". A string takes
/// the effective column's canonicalization, so both sides render it alike.
fn requested_flag_group(spec: &serde_json::Value, key: &str) -> Option<String> {
    match spec.get(key) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(group)) => {
            pipette_core::canonical_json::canonicalize_flags(Some(group))
        }
        Some(group) => Some(descriptor_from_spec(group)).filter(|flags| flags != "{}"),
    }
}

/// A `spec` object as a canonical JSON descriptor string. A gated model's
/// access token must never reach the warehouse.
fn descriptor_from_spec(spec_field: &serde_json::Value) -> String {
    pipette_plan_types::canonical_json(&without_auth_tokens(spec_field))
}

/// Removed, not redacted, to match a client's `Model::without_auth_token()`.
/// Still, never join this against a client descriptor: the client's typed
/// round-trip drops fields this keeps.
pub(crate) fn without_auth_tokens(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => fields
            .iter()
            .filter(|(key, _)| *key != "auth_token")
            .map(|(key, value)| (key.clone(), without_auth_tokens(value)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(without_auth_tokens)
            .collect::<Vec<_>>()
            .into(),
        leaf @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_)) => leaf.clone(),
    }
}

/// The only form of a job source a route may serve; the private field enforces
/// it, as the stored source can carry an access token.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ServedJobSource(serde_json::Value);

impl ServedJobSource {
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }

    /// Keep the fields every plan sharing the job agreed on under the old merge
    /// rule; [`FleetDetail::Withheld`] also drops the eligibility fields.
    pub fn from_stored(stored: &serde_json::Value, detail: FleetDetail) -> Self {
        const SHARED: [&str; 3] = ["job_id", "expires_at", "spec"];
        const ELIGIBILITY: [&str; 6] = [
            "requires",
            "any_of",
            "clients",
            "excludes",
            "not_all_of",
            "exclude_clients",
        ];
        let eligibility: &[&str] = match detail {
            FleetDetail::Full => &ELIGIBILITY,
            FleetDetail::Withheld => &[],
        };
        let fields = SHARED
            .iter()
            .chain(eligibility)
            .filter_map(|&field| {
                stored
                    .get(field)
                    .map(|value| (field.to_string(), value.clone()))
            })
            .collect::<serde_json::Map<_, _>>();
        Self(without_auth_tokens(&serde_json::Value::Object(fields)))
    }
}

/// A job body from which no failure record can ever be built, so the caller
/// deletes it rather than retry forever. Not raised for an unknown
/// `benchmark_id`: restoring the definition fixes that. Ingestion refuses such
/// a body, so no plan lists it and deleting it stalls none.
#[derive(Debug, thiserror::Error)]
#[error("job body cannot produce a failure record: {0}")]
pub struct UnrecordableJobError(String);

/// Build a terminal failure from a `todo/` job body, attributed to the reserved
/// `"system"` client.
///
/// The `model_*` / `runtime_*` labels stay unset: they would need parsing the
/// partner-defined schemas in `spec`; the descriptors carry them losslessly.
pub fn system_failure_from_job_body(
    job_body: &serde_json::Value,
    benchmark_type: BenchmarkType,
    failure_reason: impl Into<String>,
    failure_source: FailureSource,
    submitted_at: DateTime<Utc>,
) -> anyhow::Result<Submission> {
    let job_id = job_body
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| UnrecordableJobError("missing required field `job_id`".into()))?;
    let spec = job_body
        .get("spec")
        .ok_or_else(|| UnrecordableJobError("missing required field `spec`".into()))?;
    let benchmark_id = BenchmarkId::try_new(
        spec.get("benchmark")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                UnrecordableJobError("`spec` missing required field `benchmark`".into())
            })?,
    )
    .map_err(|e| UnrecordableJobError(format!("invalid `spec.benchmark`: {e}")))?;

    let wire = FailureInput {
        benchmark_id,
        retriable: false,
        failure_reason: NonEmptyTrimmedString::try_new(failure_reason.into())?,
        model_name: None,
        model_quant: None,
        // Else a null model canonicalizes to `"null"` and groups as a real cell.
        model_descriptor: spec
            .get("model")
            .filter(|v| !v.is_null())
            .map(descriptor_from_spec),
        model_flags: None,
        runtime_name: None,
        runtime_version: None,
        runtime_descriptor: spec
            .get("runtime")
            .filter(|v| !v.is_null())
            .map(descriptor_from_spec),
        runtime_flags: None,
        client_version: None,
        device_time_ms: None,
        client_extras: None,
        runtime_output: RuntimeOutput::default(),
    };

    let requested = requested_flags_from_spec(spec);

    Ok(Submission::Failure(Box::new(FailureSubmission {
        wire,
        client_id: ClientId::try_new(warehouse::SYSTEM_CLIENT_ID)?,
        job_id: JobId::try_new(job_id)
            .map_err(|e| UnrecordableJobError(format!("invalid `job_id`: {e}")))?,
        submitted_at,
        benchmark_type,
        requested_model_flags: requested.model,
        requested_runtime_flags: requested.runtime,
        requested_benchmark_flags: requested.benchmark,
        failure_source: Some(failure_source),
    })))
}

/// The one routing rule for a client submission: a held body goes to the
/// `unverified/` archive (output inline), any other to `incoming/` with its
/// output moved out. Not for synthetic failures: see [`record_system_failure`].
pub async fn write_submission_record(
    store: &dyn SubmissionStore,
    submission: &Submission,
    mut body: serde_json::Value,
    held: bool,
) -> anyhow::Result<()> {
    let job_id = submission.job_id();
    let client_id = match submission {
        Submission::Success(s) => &s.client_id,
        Submission::Failure(f) => &f.client_id,
    };
    if held {
        store
            .write_unverified(client_id, job_id, &body)
            .await
            .context("failed to write unverified submission")?;
        tracing::info!(job_id = %job_id, client_id = %client_id, "submission held in unverified");
        return Ok(());
    }
    move_runtime_output(store, job_id, client_id, &mut body).await?;
    store
        .write_incoming(job_id, &body)
        .await
        .context("failed to write submission")?;
    tracing::info!(job_id = %job_id, "submission written to incoming");
    Ok(())
}

pub async fn record_submission(
    store: &dyn SubmissionStore,
    submission: &Submission,
    held: bool,
) -> anyhow::Result<()> {
    let body = serde_json::to_value(submission).context("failed to serialize the submission")?;
    write_submission_record(store, submission, body, held).await
}

const RUNTIME_OUTPUT_KEYS: [&str; 2] = ["runtime_stdout", "runtime_stderr"];

/// Replaces any output the client wrote before.
pub async fn write_runtime_output(
    store: &dyn SubmissionStore,
    job_id: &JobId,
    client_id: &ClientId,
    output: &RuntimeOutput,
) -> anyhow::Result<()> {
    if output.is_empty() {
        return Ok(());
    }
    store
        .write_runtime_output(job_id, client_id, output)
        .await
        .context("failed to write the runtime output")
}

/// Ahead of the body's write, so a stored result never exists without its output.
pub async fn move_runtime_output(
    store: &dyn SubmissionStore,
    job_id: &JobId,
    client_id: &ClientId,
    body: &mut serde_json::Value,
) -> anyhow::Result<()> {
    let Some(fields) = body.as_object_mut() else {
        return Ok(());
    };
    let taken: serde_json::Map<String, serde_json::Value> = RUNTIME_OUTPUT_KEYS
        .iter()
        .filter_map(|key| fields.remove_entry(*key))
        .collect();
    let output: RuntimeOutput = serde_json::from_value(serde_json::Value::Object(taken))
        .context("malformed runtime output")?;
    write_runtime_output(store, job_id, client_id, &output).await
}

/// Record a synthetic `"system"` failure for a `todo/` job.
///
/// The write is exclusive: a client's real submission may land on the same key
/// after the caller's check, and must not become "nobody ran this".
pub async fn record_system_failure(
    store: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    job_body: &serde_json::Value,
    failure_reason: impl Into<String>,
    failure_source: FailureSource,
    now: DateTime<Utc>,
) -> anyhow::Result<CreateOutcome> {
    // Permanent: no catalog change makes this body recordable.
    let benchmark_id = job_body
        .get("spec")
        .and_then(|spec| spec.get("benchmark"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| UnrecordableJobError("`spec.benchmark` is absent or not a string".into()))?;
    let benchmark_id = BenchmarkId::try_new(benchmark_id)
        .map_err(|e| UnrecordableJobError(format!("invalid `spec.benchmark`: {e}")))?;
    // Retriable: restoring the definition fixes it, and the retries surface the
    // misconfiguration through cron monitoring.
    let benchmark_type = catalog
        .get(&benchmark_id)
        .map(|b| b.benchmark_type())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot resolve benchmark_type for synthetic failure (spec.benchmark: \
                 {benchmark_id}); restore the benchmark definition or delete the job from \
                 todo/avail/ or todo/leased/"
            )
        })?;
    let synthetic = system_failure_from_job_body(
        job_body,
        benchmark_type,
        failure_reason,
        failure_source,
        now,
    )?;
    let body = serde_json::to_value(&synthetic)?;
    let outcome = store
        .write_incoming_new(synthetic.job_id(), &body)
        .await
        .context("failed to write synthetic failure submission")?;
    match outcome {
        CreateOutcome::Created => tracing::info!(
            job_id = %synthetic.job_id(),
            failure_source = %failure_source,
            "synthetic system failure written to incoming"
        ),
        CreateOutcome::AlreadyExists => tracing::info!(
            job_id = %synthetic.job_id(),
            failure_source = %failure_source,
            "synthetic system failure not written; a submission already holds the job's key"
        ),
    }
    Ok(outcome)
}

/// Defaults a missing `message_type` to `"success"`: ~20k legacy bodies in
/// production lack it, and were never backfilled.
pub fn parse_stored_submission(body: &serde_json::Value) -> Result<Submission, serde_json::Error> {
    if body.get("message_type").is_some() {
        return serde_json::from_value(body.clone());
    }
    let mut with_tag = body.clone();
    if let Some(obj) = with_tag.as_object_mut() {
        obj.insert(
            "message_type".to_string(),
            serde_json::Value::String("success".to_string()),
        );
    }
    serde_json::from_value(with_tag)
}

#[cfg(test)]
pub(crate) const FIXTURE_DIGEST: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";

#[cfg(test)]
pub(crate) fn vl_spec_decode_benchmark() -> anyhow::Result<Benchmark> {
    Benchmark::from_toml(
        "vl_spec_decode_corpus_test_k8",
        &format!(
            r#"
benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "{FIXTURE_DIGEST}"
bytes   = 1024
subset  = "GQA"

[dataset.upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222"
"#
        ),
    )
}

/// With speculation on, both accept-length figures are 4.0 and it decodes at
/// 2.26x the rate of the run with it off, over the same 723 turns.
#[cfg(test)]
pub(crate) fn vl_spec_decode_wire(speculative: bool) -> serde_json::Value {
    let flags = serde_json::json!({ "speculative_enabled": speculative }).to_string();
    let mut wire = serde_json::json!({
        "runtime_flags": flags,
        "vl_spec_decode_turns": 723,
        "vl_spec_decode_prompt_ms": 79_000.0,
        "vl_spec_decode_ttft_ms": 79_530.0,
        "vl_spec_decode_generated_tokens": 14_000,
        "observation_output_digest": "3".repeat(64),
    });
    let mode = if speculative {
        serde_json::json!({
            "vl_spec_decode_predicted_ms": 100_000.0,
            "vl_spec_decode_decode_ms": 101_220.0,
            "vl_spec_decode_e2e_ms": 187_980.0,
            "vl_spec_decode_drafted_tokens": 28_000,
            "vl_spec_decode_accepted_tokens": 10_500,
            "vl_spec_decode_turn_accept_length_sum": 2_892.0,
        })
    } else {
        serde_json::json!({
            "vl_spec_decode_predicted_ms": 226_000.0,
            "vl_spec_decode_decode_ms": 227_745.0,
            "vl_spec_decode_e2e_ms": 314_505.0,
        })
    };
    if let (Some(base), serde_json::Value::Object(extra)) = (wire.as_object_mut(), mode) {
        base.extend(extra);
    }
    wire
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use pipette_plan_types::{
        AndroidThermalStatus, AndroidThrottlingSeverity, AppleThermalState,
        MAX_RUNTIME_OUTPUT_BYTES,
    };

    use super::*;

    #[rstest]
    #[case::agrees(json!({"benchmark_type": "max_memory_usage", "prefill_tokens": 256}), true)]
    #[case::other_type(
        json!({"benchmark_type": "prefill_throughput", "prefill_tokens": 256, "batch_size": 1}),
        false
    )]
    #[case::other_params(json!({"benchmark_type": "max_memory_usage", "prefill_tokens": 512}), false)]
    fn declared_benchmark_params_must_match_the_catalog(
        #[case] declared: serde_json::Value,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let declared: BenchmarkParams = serde_json::from_value(declared)?;
        let catalog: BenchmarkParams = serde_json::from_value(
            json!({"benchmark_type": "max_memory_usage", "prefill_tokens": 256}),
        )?;
        let verdict = check_declared_benchmark_params(&declared, &catalog);
        assert_eq!(verdict.is_ok(), accepted, "{verdict:?}");
        Ok(())
    }

    /// A pinned knob, the resolved temperature included, must match.
    #[rstest]
    #[case::unpinned_knobs(
        json!({"temperature": 0.6, "mcq_choices": ["A", "B"], "top_p": 0.9, "top_k": 64, "min_p": 0.05, "seed": 3}),
        true
    )]
    #[case::pinned_top_k(json!({"temperature": 0.6, "top_k": 40}), false)]
    #[case::pinned_top_k_matches(json!({"temperature": 0.6, "top_k": 64}), true)]
    #[case::other_temperature(json!({"temperature": 1.0}), false)]
    fn an_unpinned_knob_is_the_clients(
        #[case] knobs: serde_json::Value,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let eval = json!({"benchmark_type": "eval", "eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192});
        let mut catalog = eval.clone();
        catalog["temperature"] = json!(0.6);
        catalog["top_k"] = json!(64);
        let mut declared = eval;
        if let (Some(declared), Some(knobs)) = (declared.as_object_mut(), knobs.as_object()) {
            declared.extend(knobs.clone());
        }
        let catalog: BenchmarkParams = serde_json::from_value(catalog)?;
        let declared: BenchmarkParams = serde_json::from_value(declared)?;
        let verdict = check_declared_benchmark_params(&declared, &catalog);
        assert_eq!(verdict.is_ok(), accepted, "{verdict:?}");
        Ok(())
    }

    /// A knob the vlm_eval entry leaves unpinned is not the client's to pick.
    #[rstest]
    #[case::as_pinned(json!({"temperature": 0.7, "presence_penalty": 1.5}), true)]
    #[case::other_penalty(json!({"temperature": 0.7, "presence_penalty": 1.0}), false)]
    #[case::unpinned_top_p(
        json!({"temperature": 0.7, "presence_penalty": 1.5, "top_p": 0.9}),
        false
    )]
    #[case::unpinned_repeat_penalty(
        json!({"temperature": 0.7, "presence_penalty": 1.5, "repeat_penalty": 1.1}),
        false
    )]
    fn an_unpinned_vlm_eval_knob_is_not_the_clients(
        #[case] knobs: serde_json::Value,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let vlm = json!({"benchmark_type": "vlm_eval", "lmms_task": "mmstar", "dataset_version": "2026.10.1", "sample_count": 1500, "max_tokens": 8192});
        let mut catalog = vlm.clone();
        catalog["temperature"] = json!(0.7);
        catalog["presence_penalty"] = json!(1.5);
        let mut declared = vlm;
        if let (Some(declared), Some(knobs)) = (declared.as_object_mut(), knobs.as_object()) {
            declared.extend(knobs.clone());
        }
        let catalog: BenchmarkParams = serde_json::from_value(catalog)?;
        let declared: BenchmarkParams = serde_json::from_value(declared)?;
        let verdict = check_declared_benchmark_params(&declared, &catalog);
        assert_eq!(verdict.is_ok(), accepted, "{verdict:?}");
        Ok(())
    }

    /// A declaration that leaves the key out never reaches this as `None`:
    /// `BenchmarkParams::from_submission_body` fills it from the catalog.
    #[rstest]
    #[case::same(Some(32768), Some(32768), true)]
    #[case::other(Some(32768), Some(16384), false)]
    #[case::unbounded_run_on_a_bounded_entry(Some(32768), None, false)]
    #[case::bounded_run_on_an_unbounded_entry(None, Some(32768), false)]
    #[case::neither(None, None, true)]
    fn a_context_bound_is_never_the_clients(
        #[case] entry: Option<i32>,
        #[case] run: Option<i32>,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        let with = |max_context: Option<i32>| -> anyhow::Result<BenchmarkParams> {
            let mut params = json!({"benchmark_type": "eval", "eval_id": "math_500", "dataset_name": "2026.10.1", "max_tokens": 32768, "temperature": 0.6});
            if let Some(max_context) = max_context {
                params["max_context"] = json!(max_context);
            }
            Ok(serde_json::from_value(params)?)
        };
        let verdict = check_declared_benchmark_params(&with(run)?, &with(entry)?);
        assert_eq!(verdict.is_ok(), accepted, "{verdict:?}");
        Ok(())
    }

    /// A typo in the measurement definition would quietly refuse nothing.
    #[test]
    fn measurement_fields_name_real_wire_fields() -> anyhow::Result<()> {
        // Through serde, to read the field list off the type.
        let success: SuccessInput = serde_json::from_value(json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "d",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "c",
            "device_ram_bytes": 1_i64,
        }))?;
        let wire = serde_json::to_value(success)?;
        let fields = wire
            .as_object()
            .context("a serialized SuccessInput is an object")?;

        // Wire-only spellings, never on the serialized struct.
        let aliases = ["max_ram_bytes", "max_vram_bytes"];
        if let Some(missing) = benchmark::all_measurement_fields()
            .find(|f| !fields.contains_key(*f) && !aliases.contains(f))
        {
            anyhow::bail!("{missing} is declared a measurement field but is not on SuccessInput");
        }
        let declared: std::collections::HashSet<&str> =
            benchmark::all_measurement_fields().collect();
        if let Some(undeclared) = fields
            .keys()
            .find(|k| k.starts_with("observation_") && !declared.contains(k.as_str()))
        {
            anyhow::bail!("{undeclared} is on SuccessInput but not declared a measurement field");
        }

        // A field the failure wire type carries must not be one the gate refuses.
        let failure: FailureInput = serde_json::from_value(json!({
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": "OOM",
        }))?;
        let shared = serde_json::to_value(failure)?;
        let measurements: std::collections::HashSet<&str> =
            benchmark::all_measurement_fields().collect();
        if let Some(field) = shared
            .as_object()
            .context("a serialized FailureInput is an object")?
            .keys()
            .find(|f| is_success_only_field(f, &measurements))
        {
            anyhow::bail!("{field} is a FailureInput field but the cross-variant gate refuses it");
        }
        Ok(())
    }

    fn variant_body(is_failure: bool) -> serde_json::Value {
        if is_failure {
            json!({
                "message_type": "failure",
                "benchmark_id": "prefill_throughput_256",
                "job_id": "job-1",
                "retriable": false,
                "failure_reason": "OOM",
            })
        } else {
            json!({"benchmark_id": "prefill_throughput_256"})
        }
    }

    /// The other variant's known fields and nothing else; a failure is refused
    /// for any benchmark's numbers, not just its own.
    #[rstest]
    #[case::failure_device(true, json!({"device_name": "d"}), Err("device_name"))]
    #[case::failure_telemetry(
        true,
        json!({"device_apple_soc_temp_c_before": [41.5]}),
        Err("device_apple_soc_temp_c_before")
    )]
    #[case::failure_throughput_metric(
        true,
        json!({"prefill_time_ms": 34.7}),
        Err("prefill_time_ms")
    )]
    #[case::failure_other_types_metric(true, json!({"completions": []}), Err("completions"))]
    #[case::failure_legacy_alias(true, json!({"max_ram_bytes": 1}), Err("max_ram_bytes"))]
    #[case::failure_universal_observation(
        true,
        json!({"observation_max_host_bytes": 1}),
        Err("observation_max_host_bytes")
    )]
    #[case::failure_vl_total(true, json!({"total_time_ms": 394.1}), Err("total_time_ms"))]
    #[case::failure_device_time(true, json!({"device_time_ms": 1}), Ok(()))]
    #[case::failure_legacy_unknown(true, json!({"plan_id": "release-v1"}), Ok(()))]
    #[case::failure_tolerated_config(true, json!({"benchmark_flags": "{}"}), Ok(()))]
    #[case::failure_tolerated_gate_waits(true, json!({"readiness_gate_waits": "[]"}), Ok(()))]
    #[case::failure_own_fields(true, json!({"model_name": "m", "client_version": "1.2"}), Ok(()))]
    #[case::failure_output_and_extras(
        true,
        json!({"runtime_stderr": "OOM", "client_extras": "{}"}),
        Ok(())
    )]
    #[case::success_failure_reason(false, json!({"failure_reason": "OOM"}), Err("failure_reason"))]
    #[case::success_retriable(false, json!({"retriable": true}), Err("retriable"))]
    #[case::success_clean(false, json!({"model_name": "m"}), Ok(()))]
    #[case::success_output_and_extras(
        false,
        json!({"runtime_stdout": "ok", "client_extras": "{}"}),
        Ok(())
    )]
    fn wrong_variant_fields_verdict(
        #[case] is_failure: bool,
        #[case] extra: serde_json::Value,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let mut body = variant_body(is_failure);
        let obj = body.as_object_mut().context("body is an object")?;
        extra
            .as_object()
            .context("extra is an object")?
            .iter()
            .for_each(|(k, v)| {
                obj.insert(k.clone(), v.clone());
            });
        // The error ends with exactly these fields.
        let verdict = reject_wrong_variant_fields(&body, is_failure).map_err(|e| e.to_string());
        match (verdict, expected) {
            (Ok(()), Ok(())) => {}
            (Err(e), Err(fields)) => assert!(e.ends_with(&format!(": {fields}")), "{extra}: {e}"),
            (verdict, _) => anyhow::bail!("{extra}: wrong verdict: {verdict:?}"),
        }
        Ok(())
    }

    fn merged(
        mut base: serde_json::Value,
        overrides: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let serde_json::Value::Object(map) = overrides else {
            anyhow::bail!("the overrides must be a JSON object");
        };
        base.as_object_mut()
            .context("the fixture body is a JSON object")?
            .extend(map);
        Ok(base)
    }

    fn vl_spec_decode_input(
        speculative: bool,
        overrides: serde_json::Value,
    ) -> anyhow::Result<SuccessInput> {
        let device = json!({
            "benchmark_id": "vl_spec_decode_corpus_test_k8",
            "device_name": "d",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "15.4",
            "device_chip_model": "Apple M4 Max",
            "device_ram_bytes": 137_438_953_472_i64,
        });
        let body = merged(merged(device, vl_spec_decode_wire(speculative))?, overrides)?;
        Ok(serde_json::from_value(body)?)
    }

    /// Both modes, plus the edges a healthy run sits on.
    #[rstest]
    #[case::off(false, json!({}))]
    #[case::on(true, json!({}))]
    #[case::accepted_just_under_generated(
        true,
        json!({"vl_spec_decode_accepted_tokens": 13_999})
    )]
    #[case::zero_accept_sum(true, json!({"vl_spec_decode_turn_accept_length_sum": 0.0}))]
    #[case::everything_accepted_per_turn(
        true,
        json!({"vl_spec_decode_turn_accept_length_sum": 6_507.0})
    )]
    #[case::extra_flags(
        false,
        json!({"runtime_flags": r#"{"ctx_size":8192,"speculative_enabled":false}"#})
    )]
    fn vl_spec_decode_accepts_a_complete_run(
        #[case] speculative: bool,
        #[case] overrides: serde_json::Value,
    ) -> anyhow::Result<()> {
        let benchmark = vl_spec_decode_benchmark()?;
        vl_spec_decode_input(speculative, overrides)?.validate(&benchmark)?;
        Ok(())
    }

    /// A hair under from `f32` rounding is a loopback host, not a broken clock.
    #[rstest]
    #[case::equal_to_phases(json!({"vl_spec_decode_e2e_ms": 180_750.0}))]
    #[case::inside_slack(json!({"vl_spec_decode_e2e_ms": 180_700.0}))]
    fn vl_spec_decode_accepts_e2e_at_its_phases(
        #[case] overrides: serde_json::Value,
    ) -> anyhow::Result<()> {
        let benchmark = vl_spec_decode_benchmark()?;
        vl_spec_decode_input(true, overrides)?.validate(&benchmark)?;
        Ok(())
    }

    #[rstest]
    #[case("vl_spec_decode_turns")]
    #[case("vl_spec_decode_prompt_ms")]
    #[case("vl_spec_decode_predicted_ms")]
    #[case("vl_spec_decode_ttft_ms")]
    #[case("vl_spec_decode_decode_ms")]
    #[case("vl_spec_decode_e2e_ms")]
    #[case("vl_spec_decode_generated_tokens")]
    fn vl_spec_decode_requires_every_sum(
        #[case] field: &str,
        #[values(false, true)] speculative: bool,
    ) -> anyhow::Result<()> {
        let benchmark = vl_spec_decode_benchmark()?;
        let input = vl_spec_decode_input(speculative, json!({ field: serde_json::Value::Null }))?;
        assert!(
            matches!(input.validate(&benchmark), Err(ValidationError::MissingMetric(m)) if m == field),
            "{field} should be required"
        );
        Ok(())
    }

    /// An unknown mode is refused rather than guessed from the counters.
    #[rstest]
    #[case::no_flags(serde_json::Value::Null)]
    #[case::flag_absent(json!(r#"{"ctx_size":8192}"#))]
    #[case::not_json(json!("--n-gpu-layers 999"))]
    #[case::not_a_bool(json!(r#"{"speculative_enabled":"true"}"#))]
    fn vl_spec_decode_requires_the_speculation_state(
        #[case] runtime_flags: serde_json::Value,
    ) -> anyhow::Result<()> {
        let benchmark = vl_spec_decode_benchmark()?;
        let input = vl_spec_decode_input(true, json!({ "runtime_flags": runtime_flags }))?;
        assert!(matches!(
            input.validate(&benchmark),
            Err(ValidationError::MissingMetric(
                "runtime_flags.speculative_enabled"
            ))
        ));
        Ok(())
    }

    fn vl_throughput_entry() -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            "vl_throughput_384x384_32_64",
            r#"
benchmark_type = "vl_throughput"
parameter_image_width = 384
parameter_image_height = 384
parameter_text_tokens = 32
parameter_decode_tokens = 64
"#,
        )
    }

    /// A complete VL body; its phases sum to 472.3 ms.
    fn vl_throughput_body() -> serde_json::Value {
        json!({
            "benchmark_id": "vl_throughput_384x384_32_64",
            "device_name": "d",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "15.4",
            "device_chip_model": "Apple M4 Max",
            "device_ram_bytes": 137_438_953_472_i64,
            "prompt_tokens": 75,
            "image_tokens": 64,
            "prompt_ms": 352.3,
            "predicted_ms": 120.0,
        })
    }

    /// Counts fit the warehouse's `Int32` column, times their range; else a `400`.
    #[rstest]
    #[case::prompt_at_max(json!({"prompt_tokens": i64::from(i32::MAX)}), Ok(()))]
    #[case::prompt_past_max(json!({"prompt_tokens": i64::from(i32::MAX) + 1}), Err("expected i32"))]
    #[case::prompt_zero(json!({"prompt_tokens": 0}), Err("too small"))]
    #[case::image_at_max(json!({"image_tokens": i64::from(i32::MAX)}), Ok(()))]
    #[case::image_past_max(json!({"image_tokens": i64::from(i32::MAX) + 1}), Err("expected i32"))]
    #[case::image_zero(json!({"image_tokens": 0}), Err("too small"))]
    #[case::image_negative(json!({"image_tokens": -64}), Err("too small"))]
    #[case::total_absent(json!({}), Ok(()))]
    #[case::total_positive(json!({"total_time_ms": 394.1}), Ok(()))]
    #[case::total_with_stddev(json!({"total_time_ms": 394.1, "total_time_ms_stddev": 4.2}), Ok(()))]
    #[case::total_zero_stddev(json!({"total_time_ms": 394.1, "total_time_ms_stddev": 0.0}), Ok(()))]
    // No cross-field rule: the span holds one decode step fewer.
    #[case::total_below_the_phases(json!({"total_time_ms": 400.0}), Ok(()))]
    #[case::total_zero(json!({"total_time_ms": 0.0}), Err("too small"))]
    #[case::total_negative(json!({"total_time_ms": -1.0}), Err("too small"))]
    #[case::total_overflowing(json!({"total_time_ms": 1e300}), Err("finite"))]
    #[case::total_stddev_negative(json!({"total_time_ms_stddev": -0.5}), Err("too small"))]
    fn vl_throughput_fields_in_range(
        #[case] extra: serde_json::Value,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let parsed =
            serde_json::from_value::<SuccessInput>(merged(vl_throughput_body(), extra.clone())?);
        match (parsed, expected) {
            (Ok(input), Ok(())) => input.validate(&vl_throughput_entry()?)?,
            (Err(e), Err(refusal)) => assert!(e.to_string().contains(refusal), "{extra}: {e}"),
            (parsed, _) => anyhow::bail!("{extra}: wrong verdict: {parsed:?}"),
        }
        Ok(())
    }

    fn asr_body(extra: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        merged(
            json!({
                "benchmark_id": "asr_throughput_9400ms_text0_decode128",
                "device_name": "d",
                "device_form_factor": "laptop",
                "device_os_name": "macOS",
                "device_os_version": "15.4",
                "device_chip_model": "Apple M4 Max",
                "device_ram_bytes": 137_438_953_472_i64,
                "asr_prompt_tokens": 137,
                "asr_audio_tokens": 118,
                "asr_prompt_ms": 83.3,
                "asr_predicted_ms": 129.4,
                "client_extras": ASR_EXACT_EXTRAS,
            }),
            extra,
        )
    }

    /// What clients#1567 sends for a calibrated, exactly sized ASR row within
    /// the model's documented audio limit.
    const ASR_EXACT_EXTRAS: &str = r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated","asr_beyond_model_audio_limit":false}"#;

    fn asr_benchmark() -> anyhow::Result<Benchmark> {
        asr_benchmark_with_text(0)
    }

    fn asr_benchmark_with_text(text_tokens: i32) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            &format!("asr_throughput_9400ms_text{text_tokens}_decode128"),
            &format!(
                r#"
benchmark_type = "asr_throughput"
parameter_audio_duration_ms = 9400
parameter_audio_sample_rate_hz = 16000
parameter_audio_channels = 1
parameter_text_tokens = {text_tokens}
parameter_decode_tokens = 128
"#
            ),
        )
    }

    /// With text beside the clip, the prompt holds the audio, the text and the
    /// template: a count that leaves the template no room is refused.
    #[rstest]
    #[case::room_for_the_template(1000, 118, 1137, None)]
    #[case::one_token_of_template(1000, 118, 1119, None)]
    #[case::audio_and_text_are_the_prompt(1000, 118, 1118, Some("must be less than"))]
    #[case::text_alone_overfills(1000, 118, 1000, Some("the entry's 1000 text tokens"))]
    // The same body passes against an entry without text.
    #[case::no_text(0, 118, 137, None)]
    fn an_asr_body_holds_its_audio_and_text_within_the_prompt(
        #[case] text_tokens: i32,
        #[case] audio_tokens: i32,
        #[case] prompt_tokens: i32,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let input: SuccessInput = serde_json::from_value(asr_body(json!({
            "asr_audio_tokens": audio_tokens,
            "asr_prompt_tokens": prompt_tokens,
        }))?)?;
        match (
            input.validate(&asr_benchmark_with_text(text_tokens)?),
            refusal,
        ) {
            (Ok(()), None) => {}
            (Err(e), Some(expected)) => assert!(e.to_string().contains(expected), "{e}"),
            (got, refusal) => anyhow::bail!("expected {refusal:?}, got {got:?}"),
        }
        Ok(())
    }

    /// An ASR throughput body: audio within the prompt, and client_extras typed,
    /// required, and a pair a client produces.
    #[rstest]
    #[case::complete(json!({}), None)]
    #[case::with_spreads(json!({"asr_prompt_ms_stddev": 0.9, "asr_predicted_ms_stddev": 1.1}), None)]
    #[case::audio_just_under_the_prompt(json!({"asr_audio_tokens": 136}), None)]
    // The prompt holds the template too, so audio is never all of it.
    #[case::audio_is_the_prompt(json!({"asr_audio_tokens": 137}), Some("must be less than"))]
    #[case::audio_past_the_prompt(json!({"asr_audio_tokens": 138}), Some("must be less than"))]
    // How the row was produced: required and typed, every pair a client
    // produces accepted.
    #[case::upper_bound_measured(
        json!({"client_extras": r#"{"asr_context_sizing":"upper_bound","asr_audio_tokens_source":"measured","asr_beyond_model_audio_limit":false}"#}),
        None
    )]
    #[case::pinned(
        json!({"client_extras": r#"{"asr_context_sizing":"pinned","asr_audio_tokens_source":"calibrated","build":"x","asr_beyond_model_audio_limit":false}"#}),
        None
    )]
    #[case::pinned_measured(
        json!({"client_extras": r#"{"asr_context_sizing":"pinned","asr_audio_tokens_source":"measured","asr_beyond_model_audio_limit":false}"#}),
        None
    )]
    // A pair no client produces: an exact context comes from the calibrated
    // count, an upper bound only without one.
    #[case::exact_measured(
        json!({"client_extras": r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"measured"}"#}),
        Some("asr_context_sizing exact with asr_audio_tokens_source measured")
    )]
    #[case::upper_bound_calibrated(
        json!({"client_extras": r#"{"asr_context_sizing":"upper_bound","asr_audio_tokens_source":"calibrated"}"#}),
        Some("asr_context_sizing upper_bound with asr_audio_tokens_source calibrated")
    )]
    #[case::no_extras(json!({"client_extras": null}), Some("missing client_extras.asr_context_sizing"))]
    #[case::no_sizing(
        json!({"client_extras": r#"{"asr_audio_tokens_source":"calibrated"}"#}),
        Some("missing client_extras.asr_context_sizing")
    )]
    #[case::no_source(
        json!({"client_extras": r#"{"asr_context_sizing":"exact"}"#}),
        Some("missing client_extras.asr_audio_tokens_source")
    )]
    #[case::unknown_sizing(
        json!({"client_extras": r#"{"asr_context_sizing":"guessed","asr_audio_tokens_source":"calibrated"}"#}),
        Some("client_extras.asr_context_sizing is \"guessed\"")
    )]
    // Whether the clip is past the model's documented audio limit: required,
    // and a boolean.
    #[case::beyond_the_limit(
        json!({"client_extras": r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated","asr_beyond_model_audio_limit":true}"#}),
        None
    )]
    #[case::within_the_limit(
        json!({"client_extras": r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated","asr_beyond_model_audio_limit":false}"#}),
        None
    )]
    #[case::limit_not_a_boolean(
        json!({"client_extras": r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated","asr_beyond_model_audio_limit":"true"}"#}),
        Some("client_extras.asr_beyond_model_audio_limit is \"true\", not one of true, false")
    )]
    #[case::no_limit(
        json!({"client_extras": r#"{"asr_context_sizing":"exact","asr_audio_tokens_source":"calibrated"}"#}),
        Some("missing client_extras.asr_beyond_model_audio_limit")
    )]
    // The shape and size are held before any ASR key is read.
    #[case::extras_not_an_object(
        json!({"client_extras": "[1]"}),
        Some("client_extras is present but not valid JSON: expected a JSON object")
    )]
    #[case::extras_too_large(
        json!({"client_extras": format!(r#"{{"x":"{}"}}"#, "y".repeat(MAX_CLIENT_EXTRAS_BYTES))}),
        Some("client_extras is over its 16384-byte limit")
    )]
    fn an_asr_body_is_validated(
        #[case] extra: serde_json::Value,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let input: SuccessInput = serde_json::from_value(asr_body(extra)?)?;
        let got = input.validate(&asr_benchmark()?);
        match (got, refusal) {
            (Ok(()), None) => {}
            (Err(e), Some(expected)) => assert!(e.to_string().contains(expected), "{e}"),
            (got, refusal) => anyhow::bail!("expected {refusal:?}, got {got:?}"),
        }
        Ok(())
    }

    #[rstest]
    #[case("asr_prompt_tokens")]
    #[case("asr_audio_tokens")]
    #[case("asr_prompt_ms")]
    #[case("asr_predicted_ms")]
    fn an_asr_body_missing_a_measurement_is_refused(
        #[case] field: &'static str,
    ) -> anyhow::Result<()> {
        let mut body = asr_body(json!({}))?;
        body.as_object_mut().context("a body")?.remove(field);
        let input: SuccessInput = serde_json::from_value(body)?;
        let err = input
            .validate(&asr_benchmark()?)
            .err()
            .context("a body missing a measurement was accepted")?;
        assert_eq!(err.to_string(), format!("missing {field}"));
        Ok(())
    }

    /// A corpus eval that did not run the corpus its entry pins can never score,
    /// so it is refused at intake rather than queued.
    #[rstest]
    #[case::vlm_the_pinned_corpus("vlm_eval", Some("1"), None)]
    #[case::vlm_none_reported("vlm_eval", None, Some("observation_dataset_digest (none reported)"))]
    #[case::vlm_another_corpus("vlm_eval", Some("3"), Some("observation_dataset_digest 333"))]
    #[case::asr_the_pinned_corpus("asr_eval", Some("1"), None)]
    #[case::asr_none_reported("asr_eval", None, Some("observation_dataset_digest (none reported)"))]
    #[case::asr_another_corpus("asr_eval", Some("3"), Some("observation_dataset_digest 333"))]
    fn a_corpus_eval_body_reports_the_pinned_corpus(
        #[case] benchmark_type: &str,
        #[case] reported: Option<&str>,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let tables = if benchmark_type == "vlm_eval" {
            "parameter_lmms_task = \"mmstar\"\n".to_owned()
                + &pipette_core::dataset::vlm_eval_catalog_tables("mmstar", "2026.10.1", None)
        } else {
            "parameter_asr_task = \"asr_fleurs_nl_nl\"\nparameter_prompt_profile = \"voxtral\"\n"
                .to_owned()
                + &pipette_core::dataset::asr_eval_catalog_tables(
                    "asr_fleurs_nl_nl",
                    "2026.10.1",
                    "voxtral",
                )
        };
        let benchmark = Benchmark::from_toml(
            "corpus_eval",
            &format!(
                "benchmark_type = \"{benchmark_type}\"\nparameter_dataset_version = \"2026.10.1\"\n\
                 parameter_sample_count = 1\nparameter_max_tokens = 64\n{tables}"
            ),
        )?;
        let mut body = json!({
            "benchmark_id": "corpus_eval",
            "device_name": "d",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "15.4",
            "device_chip_model": "Apple M4 Max",
            "device_ram_bytes": 137_438_953_472_i64,
            "completions": [{"id": "s1", "completion": "B"}],
        });
        if let Some(digit) = reported {
            body["observation_dataset_digest"] = json!(digit.repeat(64));
        }
        let input: SuccessInput = serde_json::from_value(body)?;
        match (input.validate(&benchmark), refusal) {
            (Ok(()), None) => {}
            (Err(e), Some(expected)) => assert!(e.to_string().contains(expected), "{e}"),
            (got, refusal) => anyhow::bail!("expected {refusal:?}, got {got:?}"),
        }
        Ok(())
    }

    /// An ASR token count is positive and fits the warehouse's `Int32` column,
    /// or the body does not deserialize, for the reason named.
    #[rstest]
    #[case::prompt_at_max("asr_prompt_tokens", i64::from(i32::MAX), Ok(()))]
    #[case::prompt_past_max("asr_prompt_tokens", i64::from(i32::MAX) + 1, Err("expected i32"))]
    #[case::prompt_zero("asr_prompt_tokens", 0, Err("too small"))]
    #[case::audio_zero("asr_audio_tokens", 0, Err("too small"))]
    #[case::audio_negative("asr_audio_tokens", -5, Err("too small"))]
    fn asr_token_counts_are_positive_32_bit(
        #[case] field: &'static str,
        #[case] value: i64,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let body = asr_body(json!({ field: value }))?;
        match (serde_json::from_value::<SuccessInput>(body), expected) {
            (Ok(_), Ok(())) => {}
            (Err(e), Err(refusal)) => assert!(e.to_string().contains(refusal), "{field}: {e}"),
            (parsed, _) => anyhow::bail!("{field} = {value}: wrong verdict: {parsed:?}"),
        }
        Ok(())
    }

    fn asr_memory_benchmark() -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            "asr_max_memory_9400ms_text0_decode256",
            r#"
benchmark_type = "asr_max_memory"
parameter_audio_duration_ms = 9400
parameter_audio_sample_rate_hz = 16000
parameter_audio_channels = 1
parameter_decode_tokens = 256
"#,
        )
    }

    /// An ASR peak says how its context was sized and whether its clip is past
    /// the model's documented audio limit; the audio-count source is
    /// throughput's alone, so a memory row does not need it.
    #[rstest]
    #[case::exact(
        Some(r#"{"asr_context_sizing":"exact","asr_beyond_model_audio_limit":false}"#),
        None
    )]
    #[case::upper_bound(
        Some(r#"{"asr_context_sizing":"upper_bound","asr_beyond_model_audio_limit":false}"#),
        None
    )]
    #[case::no_extras(None, Some("missing client_extras.asr_context_sizing"))]
    #[case::not_an_object(Some("[1]"), Some("expected a JSON object"))]
    #[case::other_keys_only(
        Some(r#"{"build":"nightly"}"#),
        Some("missing client_extras.asr_context_sizing")
    )]
    #[case::unknown(
        Some(r#"{"asr_context_sizing":"approx"}"#),
        Some("not one of exact, upper_bound, pinned")
    )]
    #[case::beyond_the_limit(
        Some(r#"{"asr_context_sizing":"exact","asr_beyond_model_audio_limit":true}"#),
        None
    )]
    #[case::limit_not_a_boolean(
        Some(r#"{"asr_context_sizing":"exact","asr_beyond_model_audio_limit":1}"#),
        Some("client_extras.asr_beyond_model_audio_limit is 1")
    )]
    #[case::no_limit(
        Some(r#"{"asr_context_sizing":"exact"}"#),
        Some("missing client_extras.asr_beyond_model_audio_limit")
    )]
    fn an_asr_peak_names_its_context_sizing(
        #[case] extras: Option<&str>,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let input: SuccessInput = serde_json::from_value(json!({
            "benchmark_id": "asr_max_memory_9400ms_text0_decode256",
            "device_name": "d",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "15.4",
            "device_chip_model": "Apple M4 Max",
            "device_ram_bytes": 137_438_953_472_i64,
            "max_ram_bytes": 3_158_000_000_i64,
            "client_extras": extras,
        }))?;
        match (input.validate(&asr_memory_benchmark()?), refusal) {
            (Ok(()), None) => {}
            (Err(e), Some(expected)) => assert!(e.to_string().contains(expected), "{e}"),
            (got, refusal) => anyhow::bail!("expected {refusal:?}, got {got:?}"),
        }
        Ok(())
    }

    #[rstest]
    #[case::generated_zero(false, json!({"vl_spec_decode_generated_tokens": 0}))]
    #[case::drafted_zero(true, json!({"vl_spec_decode_drafted_tokens": 0}))]
    // The phases sum to 180_750 / 307_275; the slack is 0.1% of that.
    #[case::e2e_under_phases(false, json!({"vl_spec_decode_e2e_ms": 300_000.0}))]
    #[case::e2e_just_outside_slack(true, json!({"vl_spec_decode_e2e_ms": 180_500.0}))]
    #[case::over_drafted(true, json!({"vl_spec_decode_accepted_tokens": 28_001}))]
    #[case::negative_accepted(true, json!({"vl_spec_decode_accepted_tokens": -1}))]
    #[case::accepted_equals_generated(true, json!({"vl_spec_decode_accepted_tokens": 14_000}))]
    #[case::accepted_over_generated(true, json!({"vl_spec_decode_accepted_tokens": 14_001}))]
    // Between 0 and `turns * (k + 1)`, here `turns * 9`.
    #[case::negative_accept_sum(
        true,
        json!({"vl_spec_decode_turn_accept_length_sum": -0.5})
    )]
    #[case::accept_sum_over_k_plus_one(
        true,
        json!({"vl_spec_decode_turn_accept_length_sum": 6_507.5})
    )]
    #[case::accept_sum_overflowing(
        true,
        json!({"vl_spec_decode_turn_accept_length_sum": 1e300})
    )]
    #[case::drafted_alone(true, json!({"vl_spec_decode_drafted_tokens": null}))]
    #[case::accepted_alone(true, json!({"vl_spec_decode_accepted_tokens": null}))]
    #[case::accept_sum_alone(true, json!({"vl_spec_decode_turn_accept_length_sum": null}))]
    #[case::drafter_counters_with_speculation_off(
        true,
        json!({"runtime_flags": r#"{"speculative_enabled":false}"#})
    )]
    #[case::no_drafter_counters_with_speculation_on(
        false,
        json!({"runtime_flags": r#"{"speculative_enabled":true}"#})
    )]
    fn vl_spec_decode_rejects_an_out_of_range_counter(
        #[case] speculative: bool,
        #[case] overrides: serde_json::Value,
    ) -> anyhow::Result<()> {
        let benchmark = vl_spec_decode_benchmark()?;
        let input = vl_spec_decode_input(speculative, overrides)?;
        assert!(matches!(
            input.validate(&benchmark),
            Err(ValidationError::VlSpecDecodeOutOfRange { .. })
        ));
        Ok(())
    }

    /// A zero phase is a real reading. `1e300` arrives as `inf`.
    #[rstest]
    #[case::turns_zero(json!({"vl_spec_decode_turns": 0}), false)]
    #[case::predicted_zero(json!({"vl_spec_decode_predicted_ms": 0.0}), false)]
    #[case::negative_predicted(json!({"vl_spec_decode_predicted_ms": -1.0}), false)]
    #[case::overflowing_predicted(json!({"vl_spec_decode_predicted_ms": 1e300}), false)]
    #[case::e2e_zero(json!({"vl_spec_decode_e2e_ms": 0.0}), false)]
    #[case::negative_prompt(json!({"vl_spec_decode_prompt_ms": -1.0}), false)]
    #[case::negative_ttft(json!({"vl_spec_decode_ttft_ms": -1.0}), false)]
    #[case::overflowing_decode(json!({"vl_spec_decode_decode_ms": 1e300}), false)]
    #[case::zero_prompt(json!({"vl_spec_decode_prompt_ms": 0.0}), true)]
    #[case::zero_ttft(json!({"vl_spec_decode_ttft_ms": 0.0}), true)]
    fn vl_spec_decode_timing_range(
        #[case] overrides: serde_json::Value,
        #[case] accepted: bool,
        #[values(false, true)] speculative: bool,
    ) -> anyhow::Result<()> {
        match vl_spec_decode_input(speculative, overrides.clone()) {
            Ok(input) if accepted => input.validate(&vl_spec_decode_benchmark()?)?,
            Ok(_) => anyhow::bail!("{overrides} should not deserialize"),
            Err(e) if accepted => anyhow::bail!("{overrides} should deserialize: {e}"),
            Err(_) => {}
        }
        Ok(())
    }

    #[rstest]
    #[case::dataset_truncated("observation_dataset_digest", "deadbeef")]
    #[case::dataset_prefixed(
        "observation_dataset_digest",
        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
    )]
    #[case::dataset_empty("observation_dataset_digest", "")]
    #[case::output_truncated("observation_output_digest", "deadbeef")]
    #[case::output_empty("observation_output_digest", "")]
    fn vl_spec_decode_rejects_a_malformed_digest(#[case] field: &str, #[case] bad: &str) {
        assert!(
            vl_spec_decode_input(true, json!({ field: bad })).is_err(),
            "{bad:?} should not deserialize as {field}"
        );
    }

    #[test]
    fn wrong_variant_error_names_the_fields_in_order() -> anyhow::Result<()> {
        let body = json!({
            "message_type": "failure",
            "prefill_time_ms": 1.0,
            "device_name": "d",
        });
        let Err(err) = reject_wrong_variant_fields(&body, true) else {
            anyhow::bail!("a failure body with a device and a metric field must be refused");
        };
        assert_eq!(
            err.to_string(),
            "failure submission must not carry device or measurement fields: \
             device_name, prefill_time_ms"
        );
        Ok(())
    }

    /// serde writes the stored body and `AsRef<str>` the warehouse column, so
    /// they must agree.
    #[test]
    fn failure_source_spells_one_value_everywhere() -> anyhow::Result<()> {
        for variant in <FailureSource as strum::IntoEnumIterator>::iter() {
            let json = serde_json::to_string(&variant)?;
            let wire = json.trim_matches('"');
            assert_eq!(wire, variant.as_ref(), "serde and strum disagree");
            assert_eq!(
                wire.parse::<FailureSource>()
                    .map_err(|e: strum::ParseError| anyhow::anyhow!(e))?,
                variant
            );
            assert_eq!(serde_json::from_str::<FailureSource>(&json)?, variant);
        }
        Ok(())
    }

    #[test]
    fn parse_stored_submission_defaults_missing_tag_to_success() -> anyhow::Result<()> {
        let body = json!({
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "chip",
            "device_ram_bytes": 16_000_000_000_i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        });
        let parsed = parse_stored_submission(&body).context("should parse")?;
        assert!(matches!(parsed, Submission::Success(_)));
        Ok(())
    }

    #[test]
    fn parse_success_reads_optional_os_build_and_security_patch() -> anyhow::Result<()> {
        let with_fields = json!({
            "message_type": "success",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "embedded",
            "device_os_name": "Android",
            "device_os_version": "15",
            "device_os_build": "AP3A.240905.015.A2",
            "device_os_security_patch": "2025-06-01",
            "device_cpu_arch": "aarch64",
            "device_os_family": "Linux",
            "device_chip_model": "chip",
            "device_ram_bytes": 16_000_000_000_i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        });
        let Submission::Success(s) =
            parse_stored_submission(&with_fields).context("should parse")?
        else {
            anyhow::bail!("expected success");
        };
        assert_eq!(
            s.wire.device_os_build.clone().map(String::from),
            Some("AP3A.240905.015.A2".to_string()),
        );
        assert_eq!(
            s.wire.device_os_security_patch.clone().map(String::from),
            Some("2025-06-01".to_string()),
        );
        assert_eq!(
            s.wire.device_cpu_arch.as_ref().map(|v| v.as_str()),
            Some("aarch64")
        );
        assert_eq!(
            s.wire.device_os_family.as_ref().map(|v| v.as_str()),
            Some("Linux")
        );
        let stored = serde_json::to_value(&s)?;
        assert_eq!(stored["device_cpu_arch"], "aarch64");
        assert_eq!(stored["device_os_family"], "Linux");
        let flags = pipette_core::client::recorded_run_flags(&stored);
        assert!(flags.contains("arch:arm64"));
        assert!(flags.contains("os_family:linux"));

        // Clients that predate the fields.
        let mut without = with_fields;
        let obj = without.as_object_mut().context("object")?;
        obj.remove("device_os_build");
        obj.remove("device_os_security_patch");
        obj.remove("device_cpu_arch");
        obj.remove("device_os_family");
        let Submission::Success(s) = parse_stored_submission(&without).context("should parse")?
        else {
            anyhow::bail!("expected success");
        };
        assert_eq!(s.wire.device_os_build, None);
        assert_eq!(s.wire.device_os_security_patch, None);
        assert_eq!(s.wire.device_cpu_arch, None);
        assert_eq!(s.wire.device_os_family, None);
        Ok(())
    }

    /// A stray legacy `plan_id` is ignored (no `deny_unknown_fields`).
    #[test]
    fn parse_stored_submission_honors_explicit_failure_tag() -> anyhow::Result<()> {
        let body = json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "plan_id": "release-v1-evals-missing",
            "retriable": false,
            "failure_reason": "OOM",
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
        });
        let parsed = parse_stored_submission(&body).context("should parse")?;
        assert!(matches!(parsed, Submission::Failure(_)));
        Ok(())
    }

    fn job_body_with_spec(
        model: serde_json::Value,
        runtime: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "job_id": "j1",
            "spec": {
                "benchmark": "prefill_throughput_256",
                "model": model,
                "runtime": runtime,
            },
        })
    }

    /// Attributed to `"system"`, non-retriable, descriptors stored canonically.
    #[test]
    fn system_failure_from_job_body_derives_descriptors_from_the_spec() -> anyhow::Result<()> {
        let job_body = json!({
            "job_id": "550e8400-e29b-41d4-a716-446655440000",
            "expires_at": "never",
            "clients": ["one", "two"],
            "spec": {
                "benchmark": "prefill_throughput_256",
                // Not in sorted key order: canonicalization sorts.
                "model": {"type": "gguf_text", "source": "huggingface", "org": "o", "repo_name": "r", "path": "m.gguf"},
                "runtime": {"type": "llamacpp_cli_stock_tools", "flavor": "macos-arm64"},
            },
        });
        let at = "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?;
        let submission = system_failure_from_job_body(
            &job_body,
            BenchmarkType::PrefillThroughput,
            "All eligible clients reported failure",
            FailureSource::AllClientsDenied,
            at,
        )?;
        let Submission::Failure(f) = submission else {
            anyhow::bail!("expected a failure submission");
        };
        assert_eq!(f.client_id.as_str(), "system");
        assert_eq!(f.job_id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(f.submitted_at, at);
        assert!(!f.wire.retriable);
        assert_eq!(
            f.wire.failure_reason.as_str(),
            "All eligible clients reported failure"
        );
        assert_eq!(f.wire.benchmark_id.as_str(), "prefill_throughput_256");
        assert_eq!(
            f.wire.model_descriptor.as_deref(),
            Some(
                r#"{"org":"o","path":"m.gguf","repo_name":"r","source":"huggingface","type":"gguf_text"}"#
            )
        );
        assert_eq!(
            f.wire.runtime_descriptor.as_deref(),
            Some(r#"{"flavor":"macos-arm64","type":"llamacpp_cli_stock_tools"}"#)
        );
        // The scalar grouping labels have no derivation from an opaque spec.
        assert!(f.wire.model_name.is_none());
        assert!(f.wire.model_quant.is_none());
        assert!(f.wire.runtime_name.is_none());
        assert!(f.wire.runtime_version.is_none());
        Ok(())
    }

    /// The one loss in the retirement path no later run would repair.
    #[tokio::test]
    async fn record_system_failure_never_overwrites_a_landed_submission() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = pipette_store_fs::LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let job_id = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440000");
        let catalog = HashMap::from([(
            BenchmarkId::try_new("prefill_throughput_256")?,
            Benchmark::from_toml(
                "prefill_throughput_256",
                "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
            )?,
        )]);
        let job_body = json!({
            "job_id": job_id.as_str(),
            "expires_at": "never",
            "spec": { "benchmark": "prefill_throughput_256" },
        });

        let measured = json!({
            "job_id": job_id.as_str(),
            "message_type": "success",
            "client_id": "client-1",
            "value": 123.5,
        });
        store.write_incoming(&job_id, &measured).await?;

        let outcome = record_system_failure(
            &store,
            &catalog,
            &job_body,
            "Job expired before any client completed it",
            FailureSource::Expired,
            "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?,
        )
        .await?;

        assert_eq!(outcome, CreateOutcome::AlreadyExists);
        let record = store
            .get_submission(&job_id)
            .await?
            .context("expected the client's body to survive")?;
        assert_eq!(record.body, measured);

        // A free key still records: the refusal is the race, not a blanket one.
        let other = JobId::new_unchecked("550e8400-e29b-41d4-a716-446655440001");
        let outcome = record_system_failure(
            &store,
            &catalog,
            &json!({
                "job_id": other.as_str(),
                "expires_at": "never",
                "spec": { "benchmark": "prefill_throughput_256" },
            }),
            "Job expired before any client completed it",
            FailureSource::Expired,
            "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?,
        )
        .await?;
        assert_eq!(outcome, CreateOutcome::Created);
        Ok(())
    }

    /// Not the string `"null"`, which would group every such record as one cell.
    #[rstest]
    #[case::null_model("model")]
    #[case::null_runtime("runtime")]
    fn system_failure_maps_a_null_spec_field_to_no_descriptor(
        #[case] null_field: &str,
    ) -> anyhow::Result<()> {
        let mut job_body = job_body_with_spec(
            json!({"type": "gguf_text", "source": "relative_file", "path": "m.gguf"}),
            json!({"type": "llamacpp_cli_stock_tools", "flavor": "macos-arm64"}),
        );
        job_body["spec"][null_field] = serde_json::Value::Null;

        let submission = system_failure_from_job_body(
            &job_body,
            BenchmarkType::PrefillThroughput,
            "expired",
            FailureSource::Expired,
            "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?,
        )?;
        let Submission::Failure(f) = submission else {
            anyhow::bail!("expected a failure submission");
        };
        let (nulled, kept) = match null_field {
            "model" => (&f.wire.model_descriptor, &f.wire.runtime_descriptor),
            _ => (&f.wire.runtime_descriptor, &f.wire.model_descriptor),
        };
        assert_eq!(
            nulled.as_deref(),
            None,
            "a null `{null_field}` must not become a descriptor"
        );
        assert!(
            kept.is_some(),
            "the sibling descriptor should still be derived"
        );
        Ok(())
    }

    /// The claim response still carries the token, so stripping belongs here.
    #[test]
    fn system_failure_redacts_an_auth_token_from_the_spec() -> anyhow::Result<()> {
        // Every depth: a multi-artifact model carries tokens below the top level.
        let job_body = job_body_with_spec(
            json!({
                "type": "gguf_vision",
                "source": "huggingface",
                "org": "o",
                "repo_name": "r",
                "auth_token": "hf_TOP",
                "backbone": {"path": "model.gguf", "auth_token": "hf_NESTED"},
                "extra_files": [
                    {"path": "mmproj.gguf", "auth_token": "hf_IN_ARRAY"},
                ],
            }),
            json!({"type": "mlx_macos_pipette"}),
        );
        let submission = system_failure_from_job_body(
            &job_body,
            BenchmarkType::PrefillThroughput,
            "expired",
            FailureSource::Expired,
            "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?,
        )?;
        let Submission::Failure(f) = submission else {
            anyhow::bail!("expected a failure submission");
        };

        // The whole record, not just the descriptor.
        let record = serde_json::to_string(&f.wire)?;
        for secret in ["hf_TOP", "hf_NESTED", "hf_IN_ARRAY"] {
            assert!(
                !record.contains(secret),
                "{secret} leaked into the stored record: {record}"
            );
        }

        let descriptor = f
            .wire
            .model_descriptor
            .as_deref()
            .context("model_descriptor was dropped")?;
        assert!(
            !descriptor.contains("auth_token"),
            "a redaction marker would break descriptor identity: {descriptor}"
        );
        assert_eq!(
            descriptor,
            r#"{"backbone":{"path":"model.gguf"},"extra_files":[{"path":"mmproj.gguf"}],"org":"o","repo_name":"r","source":"huggingface","type":"gguf_vision"}"#
        );
        Ok(())
    }

    /// `null` is real (`ClientRunSpec` serializes every flag field); a string
    /// renders as the effective column does, unquoted and trimmed.
    #[rstest]
    #[case::present(
        json!({"model_type": "gguf_text", "enable_thinking": true}),
        Some(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#)
    )]
    #[case::null(serde_json::Value::Null, None)]
    #[case::top_level_empty(json!({}), None)]
    #[case::nested_empty(json!({"readiness": {}}), Some(r#"{"readiness":{}}"#))]
    #[case::plain_string(json!("--n-gpu-layers 999"), Some("--n-gpu-layers 999"))]
    #[case::padded_string(json!("  --n-gpu-layers 999  "), Some("--n-gpu-layers 999"))]
    #[case::empty_string(json!(""), None)]
    #[case::string_holding_json(
        json!(r#"{"threads":8,"gpu":99}"#),
        Some(r#"{"gpu":99,"threads":8}"#)
    )]
    #[case::array(json!(["--n-gpu-layers", "999"]), Some(r#"["--n-gpu-layers","999"]"#))]
    fn requested_flags_reads_each_group_shape(
        #[case] group: serde_json::Value,
        #[case] expected: Option<&str>,
    ) {
        for key in ["model_flags", "runtime_flags", "benchmark_flags"] {
            let spec = json!({"benchmark": "b", key: group.clone()});
            let requested = requested_flags_from_spec(&spec);
            let read = match key {
                "model_flags" => requested.model,
                "runtime_flags" => requested.runtime,
                _ => requested.benchmark,
            };
            assert_eq!(read.as_deref(), expected, "{key}");
        }
    }

    #[test]
    fn requested_flags_are_absent_when_the_spec_names_none() {
        let spec = json!({"benchmark": "b", "model": {}, "runtime": {}});
        assert_eq!(requested_flags_from_spec(&spec), RequestedFlags::default());
    }

    #[test]
    fn requested_flags_drop_every_auth_token() -> anyhow::Result<()> {
        let spec = json!({
            "benchmark": "b",
            "model_flags": {
                "model_type": "gguf_text",
                "auth_token": "hf_TOP",
                "adapter": {"path": "a.gguf", "auth_token": "hf_NESTED"},
                "extra_files": [{"path": "b.gguf", "auth_token": "hf_IN_ARRAY"}],
            },
        });
        let flags = requested_flags_from_spec(&spec)
            .model
            .context("model flags were dropped")?;
        for secret in ["hf_TOP", "hf_NESTED", "hf_IN_ARRAY"] {
            assert!(!flags.contains(secret), "{secret} leaked into {flags}");
        }
        // Gone, not marked: a marker would be a second grouping bucket.
        assert_eq!(
            flags,
            r#"{"adapter":{"path":"a.gguf"},"extra_files":[{"path":"b.gguf"}],"model_type":"gguf_text"}"#
        );
        Ok(())
    }

    /// Also omits top-level fields the old merge key did not cover.
    #[test]
    fn served_job_source_drops_every_auth_token() -> anyhow::Result<()> {
        let stored = json!({
            "job_id": "job-1",
            "auth_token": "hf_TOP",
            "private_note": "owner only",
            "spec": {
                "model": {"path": "m.gguf", "auth_token": "hf_NESTED"},
                "extra_files": [{"path": "b.gguf", "auth_token": "hf_IN_ARRAY"}],
            },
        });
        let served =
            serde_json::to_value(ServedJobSource::from_stored(&stored, FleetDetail::Full))?;
        let text = serde_json::to_string(&served)?;
        for secret in [
            "auth_token",
            "hf_TOP",
            "hf_NESTED",
            "hf_IN_ARRAY",
            "owner only",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
        assert_eq!(
            served,
            json!({
                "job_id": "job-1",
                "spec": {
                    "model": {"path": "m.gguf"},
                    "extra_files": [{"path": "b.gguf"}],
                },
            })
        );
        Ok(())
    }

    #[rstest]
    #[case::full(FleetDetail::Full, true)]
    #[case::withheld(FleetDetail::Withheld, false)]
    fn served_job_source_projects_eligibility_by_tier(
        #[case] detail: FleetDetail,
        #[case] eligible: bool,
    ) -> anyhow::Result<()> {
        let shared = json!({
            "job_id": "job-1",
            "expires_at": "2026-10-01T00:00:00Z",
            "spec": {"benchmark": "eval_test"},
        });
        let eligibility = json!({
            "requires": ["gpu"],
            "any_of": [["metal"]],
            "clients": ["ev1_named"],
            "excludes": ["battery"],
            "not_all_of": [["a", "b"]],
            "exclude_clients": ["ev1_other"],
        });
        let eligibility = eligibility.as_object().context("an object")?;
        let mut stored = shared.clone();
        stored
            .as_object_mut()
            .context("an object")?
            .extend(eligibility.clone());
        let expected = if eligible { stored.clone() } else { shared };
        let served = serde_json::to_value(ServedJobSource::from_stored(&stored, detail))?;
        assert_eq!(served, expected);
        Ok(())
    }

    #[test]
    fn system_failure_records_the_requested_flags() -> anyhow::Result<()> {
        let mut job_body = job_body_with_spec(
            json!({"type": "gguf_text", "source": "relative_file", "path": "m.gguf"}),
            json!({"type": "llamacpp_cli_stock_tools", "flavor": "macos-arm64"}),
        );
        job_body["spec"]["model_flags"] =
            json!({"model_type": "gguf_text", "enable_thinking": true});
        job_body["spec"]["runtime_flags"] = json!({"runtime_type": "llamacpp", "threads": 8});
        job_body["spec"]["benchmark_flags"] = serde_json::Value::Null;

        let submission = system_failure_from_job_body(
            &job_body,
            BenchmarkType::PrefillThroughput,
            "expired",
            FailureSource::Expired,
            "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?,
        )?;
        let Submission::Failure(f) = submission else {
            anyhow::bail!("expected a failure submission");
        };
        assert_eq!(
            f.requested_model_flags.as_deref(),
            Some(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#)
        );
        assert_eq!(
            f.requested_runtime_flags.as_deref(),
            Some(r#"{"runtime_type":"llamacpp","threads":8}"#)
        );
        assert_eq!(f.requested_benchmark_flags, None);
        Ok(())
    }

    /// An error, not a record with empty strings.
    #[rstest]
    #[case::missing_job_id("job_id")]
    #[case::missing_spec("spec")]
    fn system_failure_from_job_body_errors_on_missing_required_field(
        #[case] omit: &str,
    ) -> anyhow::Result<()> {
        let mut job_body = json!({
            "job_id": "j1",
            "spec": {"benchmark": "prefill_throughput_256"},
        });
        job_body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("job body is a JSON object"))?
            .remove(omit);
        let at = "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?;
        assert!(
            system_failure_from_job_body(
                &job_body,
                BenchmarkType::PrefillThroughput,
                "reason",
                FailureSource::Expired,
                at,
            )
            .is_err()
        );
        Ok(())
    }

    /// Locks optionality in against a refactor that drops the `Option`.
    #[test]
    fn success_input_omitting_power_fields_deserializes_to_none() -> anyhow::Result<()> {
        let body = json!({
            "message_type": "success",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "chip",
            "device_ram_bytes": 16_000_000_000_i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        });
        let Submission::Success(success) = parse_stored_submission(&body)? else {
            anyhow::bail!("expected a success submission");
        };
        assert_eq!(success.wire.device_battery_level, None);
        assert_eq!(success.wire.device_power_state, None);
        assert_eq!(success.wire.device_power_save_mode, None);
        assert_eq!(success.wire.runtime_cpu_variant, None);
        assert_eq!(success.wire.device_apple_thermal_state_before, None);
        assert_eq!(success.wire.device_apple_thermal_state_after, None);
        assert_eq!(success.wire.device_apple_soc_temp_c_before, None);
        assert_eq!(success.wire.device_apple_soc_temp_c_after, None);
        assert_eq!(success.wire.device_android_thermal_status_before, None);
        assert_eq!(success.wire.device_android_thermal_status_after, None);
        assert_eq!(success.wire.device_android_thermal_headroom_before, None);
        assert_eq!(success.wire.device_android_thermal_headroom_after, None);
        assert_eq!(success.wire.device_android_thermal_sensors_before, None);
        assert_eq!(success.wire.device_android_thermal_sensors_after, None);
        assert_eq!(success.wire.device_linux_thermal_zones_before, None);
        assert_eq!(success.wire.device_linux_thermal_zones_after, None);
        assert_eq!(success.wire.device_android_cpuset, None);
        assert_eq!(success.wire.device_android_cpu_affinity_list, None);
        assert_eq!(
            success.wire.device_android_cpu_affinity_excludes_top_tier,
            None
        );
        assert_eq!(success.wire.observation_max_swap_bytes, None);
        assert_eq!(success.wire.observation_max_host_bytes, None);
        Ok(())
    }

    /// Above `2^32` (a large run's peaks exceed `i32`), on a non-memory type.
    #[test]
    fn success_input_with_observed_memory_fields_deserializes() -> anyhow::Result<()> {
        let body = json!({
            "message_type": "success",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "phone",
            "device_os_name": "Android",
            "device_os_version": "15",
            "device_chip_model": "Snapdragon 8 Elite",
            "device_ram_bytes": 12_000_000_000_i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "observation_max_swap_bytes": 2_147_483_648_i64,
            "observation_max_host_bytes": 5_368_709_120_i64,
            "prefill_time_ms": 10.0,
        });
        let Submission::Success(success) = parse_stored_submission(&body)? else {
            anyhow::bail!("expected a success submission");
        };
        assert_eq!(
            success.wire.observation_max_swap_bytes.map(i64::from),
            Some(2_147_483_648)
        );
        assert_eq!(
            success.wire.observation_max_host_bytes.map(i64::from),
            Some(5_368_709_120)
        );
        // The peak-memory benchmarks' own field stays absent.
        assert_eq!(success.wire.max_host_bytes, None);
        Ok(())
    }

    #[test]
    fn success_input_with_cpuset_fields_deserializes() -> anyhow::Result<()> {
        let body = json!({
            "message_type": "success",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "phone",
            "device_os_name": "Android",
            "device_os_version": "15",
            "device_chip_model": "Snapdragon 8 Elite",
            "device_ram_bytes": 12_000_000_000_i64,
            "device_android_cpuset": "/moderate",
            "device_android_cpu_affinity_list": "0-5",
            "device_android_cpu_affinity_excludes_top_tier": true,
            "model_name": "m",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        });
        let Submission::Success(success) = parse_stored_submission(&body)? else {
            anyhow::bail!("expected a success submission");
        };
        assert_eq!(
            success
                .wire
                .device_android_cpuset
                .as_ref()
                .map(NonEmptyTrimmedString::as_str),
            Some("/moderate")
        );
        assert_eq!(
            success
                .wire
                .device_android_cpu_affinity_list
                .as_ref()
                .map(NonEmptyTrimmedString::as_str),
            Some("0-5")
        );
        assert_eq!(
            success.wire.device_android_cpu_affinity_excludes_top_tier,
            Some(true)
        );
        Ok(())
    }

    #[test]
    fn success_input_with_thermal_fields_deserializes() -> anyhow::Result<()> {
        let body = json!({
            "message_type": "success",
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": "c1",
            "job_id": "j1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "device_name": "d",
            "device_form_factor": "phone",
            "device_os_name": "Android",
            "device_os_version": "15",
            "device_chip_model": "Snapdragon 8 Gen 3",
            "device_ram_bytes": 8_000_000_000_i64,
            "device_apple_thermal_state_before": ["nominal", "fair"],
            "device_apple_thermal_state_after": ["fair", "serious"],
            "device_apple_soc_temp_c_before": [41.5, 44.25],
            "device_apple_soc_temp_c_after": [46.0, 49.75],
            "device_android_thermal_status_before": ["none", "light"],
            "device_android_thermal_status_after": ["light", "severe"],
            "device_android_thermal_headroom_before": [0.31, 0.44],
            "device_android_thermal_headroom_after": [0.62, 0.71],
            "device_android_thermal_sensors_before": [
                {"iteration": 0, "type": "cpu", "name": "cpu-big", "celsius": 41, "throttling_status": "light"},
                {"iteration": 0, "type": "battery", "name": "batt", "celsius": 33, "throttling_status": "none"},
                {"iteration": 1, "type": "cpu", "name": "cpu-big", "celsius": 48, "throttling_status": "severe"},
            ],
            "device_linux_thermal_zones_after": [
                {"iteration": 0, "type": "x86_pkg_temp", "celsius": 58},
                {"iteration": 0, "type": "cpu-thermal", "celsius": 61},
                {"iteration": 1, "type": "x86_pkg_temp", "celsius": 63},
            ],
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        });
        let Submission::Success(success) = parse_stored_submission(&body)? else {
            anyhow::bail!("expected a success submission");
        };
        assert_eq!(
            success.wire.device_apple_thermal_state_before,
            Some(vec![AppleThermalState::Nominal, AppleThermalState::Fair])
        );
        assert_eq!(
            success.wire.device_apple_thermal_state_after,
            Some(vec![AppleThermalState::Fair, AppleThermalState::Serious])
        );
        assert_eq!(
            success.wire.device_apple_soc_temp_c_before,
            Some(vec![41.5, 44.25])
        );
        assert_eq!(
            success.wire.device_apple_soc_temp_c_after,
            Some(vec![46.0, 49.75])
        );
        assert_eq!(
            success.wire.device_android_thermal_status_before,
            Some(vec![
                AndroidThermalStatus::None,
                AndroidThermalStatus::Light
            ])
        );
        assert_eq!(
            success.wire.device_android_thermal_status_after,
            Some(vec![
                AndroidThermalStatus::Light,
                AndroidThermalStatus::Severe
            ])
        );
        assert_eq!(
            success.wire.device_android_thermal_headroom_before,
            Some(vec![0.31, 0.44])
        );
        assert_eq!(
            success.wire.device_android_thermal_headroom_after,
            Some(vec![0.62, 0.71])
        );

        // Flattened (iteration, sensor) pairs.
        let sensors = success
            .wire
            .device_android_thermal_sensors_before
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sensors present"))?;
        assert_eq!(sensors.len(), 3);
        assert_eq!(sensors[0].iteration, 0);
        assert_eq!(sensors[0].sensor_type, "cpu");
        assert_eq!(sensors[0].name, "cpu-big");
        assert_eq!(sensors[0].celsius, 41);
        assert_eq!(
            sensors[0].throttling_status,
            AndroidThrottlingSeverity::Light
        );
        assert_eq!(
            sensors[1].throttling_status,
            AndroidThrottlingSeverity::None
        );
        assert_eq!(sensors[2].iteration, 1);
        assert_eq!(sensors[2].celsius, 48);

        let zones = success
            .wire
            .device_linux_thermal_zones_after
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("zones present"))?;
        assert_eq!(zones.len(), 3);
        assert_eq!(zones[0].iteration, 0);
        assert_eq!(zones[0].zone_type, "x86_pkg_temp");
        assert_eq!(zones[0].celsius, 58);
        assert_eq!(zones[2].iteration, 1);
        assert_eq!(zones[2].celsius, 63);
        Ok(())
    }

    fn prefill_benchmark() -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            "prefill_throughput_256",
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )
    }

    fn input_body(is_failure: bool, extra: serde_json::Value) -> anyhow::Result<SubmissionInput> {
        let mut body = if is_failure {
            variant_body(true)
        } else {
            json!({
                "message_type": "success",
                "benchmark_id": "prefill_throughput_256",
                "device_name": "d",
                "device_form_factor": "embedded",
                "device_os_name": "Linux",
                "device_os_version": "22.04",
                "device_chip_model": "c",
                "device_ram_bytes": 1_i64,
                "prefill_time_ms": 34.7,
            })
        };
        let serde_json::Value::Object(extra) = extra else {
            anyhow::bail!("extra is not an object: {extra}");
        };
        body.as_object_mut()
            .context("body is an object")?
            .extend(extra);
        Ok(serde_json::from_value(body)?)
    }

    #[rstest]
    #[case::object(json!(r#"{"build":"nightly"}"#), None)]
    #[case::empty_object(json!("{}"), None)]
    #[case::array(json!("[1]"), Some("client_extras is present but not valid JSON: expected a JSON object"))]
    #[case::not_json(json!("nightly"), Some("client_extras is present but not valid JSON"))]
    #[case::at_limit(
        json!(format!(r#"{{"x":"{}"}}"#, "y".repeat(MAX_CLIENT_EXTRAS_BYTES - r#"{"x":""}"#.len()))),
        None
    )]
    #[case::too_large(
        json!(format!(r#"{{"x":"{}"}}"#, "y".repeat(MAX_CLIENT_EXTRAS_BYTES))),
        Some("client_extras is over its 16384-byte limit")
    )]
    fn client_extras_must_be_a_json_object_within_its_limit(
        #[case] extras: serde_json::Value,
        #[case] refusal: Option<&str>,
        #[values(false, true)] is_failure: bool,
    ) -> anyhow::Result<()> {
        let input = input_body(is_failure, json!({ "client_extras": extras }))?;
        match (input.validate(&prefill_benchmark()?), refusal) {
            (Ok(()), None) => {}
            (Err(e), Some(refusal)) => assert!(e.to_string().starts_with(refusal), "{e}"),
            (verdict, _) => anyhow::bail!("wrong verdict: {verdict:?}"),
        }
        Ok(())
    }

    /// A value no `Int64` column holds is refused.
    #[rstest]
    #[case::absent(json!({}), Ok(None))]
    #[case::zero(json!({ "device_time_ms": 0 }), Ok(Some(0)))]
    #[case::measured(json!({ "device_time_ms": 184_250 }), Ok(Some(184_250)))]
    #[case::negative(json!({ "device_time_ms": -1 }), Err("too small"))]
    #[case::past_max(json!({ "device_time_ms": 1_u64 << 63 }), Err("expected i64"))]
    fn device_time_ms_is_stored_when_in_range(
        #[case] extra: serde_json::Value,
        #[case] expected: Result<Option<i64>, &str>,
        #[values(false, true)] is_failure: bool,
    ) -> anyhow::Result<()> {
        let stored = input_body(is_failure, extra).and_then(|input| {
            let submission = input.into_submission(
                ClientId::try_new("ev1_a")?,
                JobId::new_unchecked("job-1"),
                Utc::now(),
                BenchmarkType::PrefillThroughput,
            );
            Ok(serde_json::to_value(&submission)?["device_time_ms"].as_i64())
        });
        match (stored, expected) {
            (Ok(stored), Ok(expected)) => assert_eq!(stored, expected),
            (Err(e), Err(refusal)) => assert!(e.to_string().contains(refusal), "{e}"),
            (stored, _) => anyhow::bail!("wrong verdict: {stored:?}"),
        }
        Ok(())
    }

    #[rstest]
    fn device_telemetry_never_refuses_a_submission(
        #[values(
            "device_hardware_readings",
            "device_os_throttling_readings",
            "readiness_gate_waits"
        )]
        field: &str,
        #[values(
            r#"[{"iteration":0,"sample_point":"before_rep"}]"#.to_owned(),
            r#"{"iteration":0}"#.to_owned(),
            "throttled=0x0".to_owned(),
            "y".repeat(MAX_DEVICE_READINGS_BYTES + 1)
        )]
        value: String,
    ) -> anyhow::Result<()> {
        input_body(false, json!({ field: value }))?.validate(&prefill_benchmark()?)?;
        Ok(())
    }

    fn ingested_wire(extra: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let Submission::Success(s) = input_body(false, extra)?.into_submission(
            ClientId::try_new("ev1_a")?,
            JobId::new_unchecked("job-1"),
            Utc::now(),
            BenchmarkType::PrefillThroughput,
        ) else {
            anyhow::bail!("a success body became a failure");
        };
        Ok(serde_json::to_value(s.wire)?)
    }

    /// Whole elements from the front, so it still parses; at the cap, as sent.
    #[rstest]
    #[case::hardware("device_hardware_readings", MAX_DEVICE_READINGS_BYTES)]
    #[case::os_throttling("device_os_throttling_readings", MAX_DEVICE_READINGS_BYTES)]
    #[case::gate_waits("readiness_gate_waits", MAX_READINESS_GATE_WAITS_BYTES)]
    fn into_submission_cuts_device_telemetry_by_element(
        #[case] field: &str,
        #[case] cap: usize,
        #[values(false, true)] over: bool,
    ) -> anyhow::Result<()> {
        // `["x…x","last rep"]`, padded to the cap or one byte past it.
        let last = r#""last rep""#;
        let pad = cap + usize::from(over) - r#"["",]"#.len() - last.len();
        let sent = format!(r#"["{}",{last}]"#, "x".repeat(pad));
        let wire = ingested_wire(json!({ field: sent }))?;
        let expected = if over { format!("[{last}]") } else { sent };
        assert_eq!(wire.get(field), Some(&json!(expected)));
        Ok(())
    }

    #[rstest]
    fn into_submission_drops_device_telemetry_that_is_not_an_array(
        #[values(
            "device_hardware_readings",
            "device_os_throttling_readings",
            "readiness_gate_waits"
        )]
        field: &str,
        #[values(r#"{"iteration":0}"#, "throttled=0x0", r#"…"iteration":4}]"#)] sent: &str,
    ) -> anyhow::Result<()> {
        let wire = ingested_wire(json!({ field: sent }))?;
        assert!(
            wire.get(field).is_none_or(serde_json::Value::is_null),
            "{wire}"
        );
        Ok(())
    }

    /// The keys [`move_runtime_output`] takes out of a body are the ones serde
    /// writes the output under.
    #[test]
    fn runtime_output_keys_are_its_wire_names() -> anyhow::Result<()> {
        let output = serde_json::to_value(RuntimeOutput {
            runtime_stdout: Some(String::new()),
            runtime_stderr: Some(String::new()),
        })?;
        let mut keys: Vec<&str> = output
            .as_object()
            .context("output is an object")?
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = RUNTIME_OUTPUT_KEYS;
        keys.sort_unstable();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        Ok(())
    }

    #[rstest]
    fn into_submission_caps_the_runtime_output(
        #[values(false, true)] is_failure: bool,
    ) -> anyhow::Result<()> {
        let long = format!("{}final error", "x".repeat(MAX_RUNTIME_OUTPUT_BYTES));
        let input = input_body(
            is_failure,
            json!({ "runtime_stdout": "ready", "runtime_stderr": long }),
        )?;
        let submission = input.into_submission(
            ClientId::try_new("ev1_a")?,
            JobId::new_unchecked("job-1"),
            Utc::now(),
            BenchmarkType::PrefillThroughput,
        );
        let output = match &submission {
            Submission::Success(s) => &s.wire.runtime_output,
            Submission::Failure(f) => &f.wire.runtime_output,
        };
        assert_eq!(output.runtime_stdout.as_deref(), Some("ready"));
        let stderr = output.runtime_stderr.as_deref().context("stderr kept")?;
        assert_eq!(stderr.len(), MAX_RUNTIME_OUTPUT_BYTES);
        assert!(stderr.ends_with("final error"), "the tail must survive");
        Ok(())
    }

    /// A held body keeps its output inline until `promote`.
    #[rstest]
    #[tokio::test]
    async fn write_submission_record_moves_the_output_out_of_an_incoming_body(
        #[values(false, true)] is_failure: bool,
        #[values(false, true)] held: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = pipette_store_fs::LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let client_id = ClientId::try_new("ev1_a")?;
        let job_id = JobId::new_unchecked("job-1");
        let submission = input_body(
            is_failure,
            json!({
                "runtime_stdout": "ready\n",
                "runtime_stderr": "",
                "client_extras": r#"{"build":"nightly"}"#,
            }),
        )?
        .into_submission(
            client_id.clone(),
            job_id.clone(),
            Utc::now(),
            BenchmarkType::PrefillThroughput,
        );
        let body = serde_json::to_value(&submission)?;

        write_submission_record(&store, &submission, body, held).await?;

        let stored = if held {
            store
                .list_unverified_client(&client_id)
                .await?
                .pop()
                .context("expected the held body")?
                .1
        } else {
            store
                .get_submission(&job_id)
                .await?
                .context("expected the incoming body")?
                .body
        };
        assert_eq!(stored["client_extras"], r#"{"build":"nightly"}"#);
        assert_eq!(stored.get("runtime_stdout").is_some(), held, "{stored}");
        assert_eq!(stored.get("runtime_stderr").is_some(), held, "{stored}");
        let expected = if held {
            vec![]
        } else {
            vec![(
                client_id,
                RuntimeOutput {
                    runtime_stdout: Some("ready\n".to_owned()),
                    runtime_stderr: Some(String::new()),
                },
            )]
        };
        assert_eq!(store.list_runtime_output(&job_id).await?, expected);
        Ok(())
    }
}
