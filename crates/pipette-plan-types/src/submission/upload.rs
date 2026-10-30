//! `POST /benchmarks`: a device's result as the server reads it, a success or
//! a failure tagged on `message_type`, and the receipts it answers with.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    BenchmarkSubmissionPayload, ByteCount, NonNegativeMillis, PositiveMillis, RuntimeOutput,
};
use crate::benchmark::declared_benchmark_params;
use crate::{
    AndroidTemperatureSensor, AndroidThermalStatus, AppleThermalState, BatteryLevel, BenchmarkId,
    BenchmarkParams, DevicePowerState, ElapsedMillis, LinuxThermalZone, NonEmptyTrimmedString,
    PositiveCount, Sha256,
};

/// `POST /benchmarks`'s body as a client sends it: the result, and the `job_id`
/// it claimed. An ad-hoc success run leaves `job_id` out and the server mints
/// one; a failure always carries it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Submission {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// What the client ran, which the server checks against its catalog.
    #[serde(flatten, with = "declared_benchmark_params")]
    pub benchmark_params: Option<BenchmarkParams>,
    #[serde(flatten)]
    pub input: SubmissionInput,
}

impl Submission {
    /// The success a client built as a payload, under the job it claimed.
    pub fn success(
        payload: &BenchmarkSubmissionPayload,
        job_id: Option<String>,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            job_id,
            benchmark_params: payload.benchmark_params.clone(),
            input: SubmissionInput::Success(Box::new(SuccessInput::try_from(payload)?)),
        })
    }
}

impl TryFrom<&BenchmarkSubmissionPayload> for SuccessInput {
    type Error = serde_json::Error;

    /// The payload read as the server reads it, so the two cannot disagree: it
    /// fails, naming the field, exactly where the server would refuse it.
    fn try_from(payload: &BenchmarkSubmissionPayload) -> Result<Self, Self::Error> {
        Self::deserialize(serde_json::to_value(payload)?)
    }
}

/// A device's result. Tagged on `message_type`; a body without one reads as a
/// success, as clients that predate the failure variant send it.
#[derive(Debug, Serialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
pub enum SubmissionInput {
    // Boxed so the enum stays small to move, though each variant is large.
    Success(Box<SuccessInput>),
    Failure(Box<FailureInput>),
}

impl SubmissionInput {
    pub fn benchmark_id(&self) -> &BenchmarkId {
        match self {
            Self::Success(s) => &s.benchmark_id,
            Self::Failure(f) => &f.benchmark_id,
        }
    }
}

impl<'de> Deserialize<'de> for SubmissionInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "message_type", rename_all = "snake_case")]
        enum Tagged {
            Success(Box<SuccessInput>),
            Failure(Box<FailureInput>),
        }

        let mut body = serde_json::Map::deserialize(deserializer)?;
        body.entry("message_type")
            .or_insert_with(|| "success".into());
        Ok(
            match Tagged::deserialize(serde_json::Value::Object(body)).map_err(D::Error::custom)? {
                Tagged::Success(s) => Self::Success(s),
                Tagged::Failure(f) => Self::Failure(f),
            },
        )
    }
}

/// A `message_type: "success"` submission: the device, model and runtime
/// context plus the metric fields of its `benchmark_type`, which the server
/// resolves from the catalog.
#[derive(Debug, Deserialize, Serialize)]
pub struct SuccessInput {
    pub benchmark_id: BenchmarkId,

    pub device_name: NonEmptyTrimmedString,
    /// A `DeviceFormFactor` name, checked by `validate` rather than at parse.
    pub device_form_factor: NonEmptyTrimmedString,
    pub device_os_name: NonEmptyTrimmedString,
    pub device_os_version: NonEmptyTrimmedString,
    /// Precise OS build identifier, finer-grained than `device_os_version`
    /// (e.g. iOS `22F76`, macOS `24F74`, Windows `26100.1234`, Android
    /// `AP3A.240905.015.A2`, Linux full `uname -r`). Opaque grouping value.
    pub device_os_build: Option<NonEmptyTrimmedString>,
    /// OS security-patch level where the platform exposes one, today Android's
    /// `Build.VERSION.SECURITY_PATCH` (e.g. `2025-06-01`).
    pub device_os_security_patch: Option<NonEmptyTrimmedString>,
    /// The CPU architecture that the device reports. The server maps known
    /// aliases to a canonical value for the `arch:` flag, for example `amd64`
    /// to `x86_64` and `aarch64` to `arm64`. It keeps other values as a
    /// lowercase slug. These examples are not the full list:
    /// `canonical_cpu_arch` in `pipette-core` holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_cpu_arch: Option<NonEmptyTrimmedString>,
    /// The OS family that the device reports. The server maps known aliases
    /// to a canonical value for the `os_family:` flag, for example `win32` to
    /// `windows` and `ipados` to `ios`. It keeps other values as a lowercase
    /// slug. These examples are not the full list: `canonical_os_family` in
    /// `pipette-core` holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_family: Option<NonEmptyTrimmedString>,
    pub device_chip_model: NonEmptyTrimmedString,
    pub device_gpu_model: Option<NonEmptyTrimmedString>,
    pub device_gpu_vram_bytes: Option<ByteCount>,
    pub device_npu_model: Option<NonEmptyTrimmedString>,
    pub device_npu_vram_bytes: Option<ByteCount>,
    pub device_ram_bytes: ByteCount,
    pub device_battery_level: Option<BatteryLevel>,
    pub device_power_state: Option<DevicePowerState>,
    pub device_power_save_mode: Option<bool>,
    // Thermal telemetry, one value per measured rep: `_before` at the rep's
    // gate-pass, `_after` once its timed work is done. Sensor and zone families
    // flatten every (rep, sensor) pair, each tagged with its `iteration`. The
    // run's worst is derived downstream, not sent.
    pub device_apple_thermal_state_before: Option<Vec<AppleThermalState>>,
    pub device_apple_thermal_state_after: Option<Vec<AppleThermalState>>,
    /// Raw iOS SoC die temperature in °C per rep, beside the Apple thermal
    /// state; only on private-thermal builds (`PIPETTE_PRIVATE_THERMAL`). No
    /// element may be null: a client that misses a rep omits the whole series.
    pub device_apple_soc_temp_c_before: Option<Vec<f32>>,
    pub device_apple_soc_temp_c_after: Option<Vec<f32>>,
    pub device_android_thermal_status_before: Option<Vec<AndroidThermalStatus>>,
    pub device_android_thermal_status_after: Option<Vec<AndroidThermalStatus>>,
    pub device_android_thermal_headroom_before: Option<Vec<f32>>,
    pub device_android_thermal_headroom_after: Option<Vec<f32>>,
    pub device_android_thermal_sensors_before: Option<Vec<AndroidTemperatureSensor>>,
    pub device_android_thermal_sensors_after: Option<Vec<AndroidTemperatureSensor>>,
    pub device_linux_thermal_zones_before: Option<Vec<LinuxThermalZone>>,
    pub device_linux_thermal_zones_after: Option<Vec<LinuxThermalZone>>,
    // Device telemetry as each OS reported it, read by nothing. Ingest keeps
    // only a JSON array, cut to its cap; see `docs/design/device-telemetry.md`.
    pub device_hardware_readings: Option<String>,
    pub device_os_throttling_readings: Option<String>,
    pub readiness_gate_waits: Option<String>,
    // Android only, one value per submission: the cpuset group (`/top-app`)
    // and CPU list (`0-3,6-7`) the benchmark ran under, so OEM demotion off the
    // prime cores shows; `_excludes_top_tier` is that demotion signal.
    pub device_android_cpuset: Option<NonEmptyTrimmedString>,
    pub device_android_cpu_affinity_list: Option<NonEmptyTrimmedString>,
    pub device_android_cpu_affinity_excludes_top_tier: Option<bool>,

    /// Human-facing model grouping and display key (e.g. `{repo}:{filename}`);
    /// a submission may carry it, [`Self::model_descriptor`], or both.
    pub model_name: Option<NonEmptyTrimmedString>,
    /// Quantization label of the primary artifact; lossy for multi-artifact
    /// models, whose per-piece quants live in `model_descriptor`.
    pub model_quant: Option<NonEmptyTrimmedString>,
    /// Total parameter count in millions; the scorer fills it from the catalog
    /// when known.
    pub model_params_total_millions: Option<PositiveCount>,
    /// Active parameter count in millions (drives throughput): the total for a
    /// dense model, set explicitly for MoE. Must not exceed the total.
    pub model_params_active_millions: Option<PositiveCount>,
    /// Lossless model specification as a **JSON string** (one artifact for an
    /// MLX bundle, several for a llama.cpp VL model). Opaque to the server,
    /// since partners define their own formats: only canonicalized
    /// ([`crate::canonical_json`]) and stored, so pattern search is stable.
    pub model_descriptor: Option<String>,
    /// Opaque model configuration, typically JSON (`{"enable_thinking":true}`)
    /// but any string; stored verbatim, never validated.
    pub model_flags: Option<NonEmptyTrimmedString>,

    /// Cheap grouping/display runtime name. Optional — the authoritative
    /// identity (name, version, build) lives in [`Self::runtime_descriptor`].
    pub runtime_name: Option<NonEmptyTrimmedString>,
    /// Cheap grouping/display version string. Optional — the authoritative
    /// version is baked into [`Self::runtime_descriptor`] when present.
    pub runtime_version: Option<NonEmptyTrimmedString>,
    /// Lossless runtime specification as a **JSON string**, version and build
    /// included; canonicalized and stored, never deserialized. See
    /// [`crate::canonical_json`].
    pub runtime_descriptor: Option<String>,
    /// Opaque runtime configuration, typically JSON but any string (e.g.
    /// `--n-gpu-layers 999`); stored verbatim, never validated.
    pub runtime_flags: Option<NonEmptyTrimmedString>,
    /// The **resolved** harness configuration (readiness gating, timeouts, loop
    /// detection) as a JSON string. Canonicalized and stored with a `_sha256`,
    /// like the descriptors: it groups runs measured the same way. A client
    /// submits the value it ran with, never null; see `docs/storage.md`.
    pub benchmark_flags: Option<String>,
    /// The host environment the client forwarded into the runtime, as a
    /// `{"envs":[...]}` JSON string. A measurement condition, so it enters no
    /// cell identity, dedup or job-matching key; stored verbatim, unhashed.
    /// Absent when the client had no entry for the runtime it ran.
    pub client_configuration: Option<String>,
    /// Runtime-selected CPU kernel variant, per `runtime_name`: for llama.cpp
    /// the `ggml-cpu-<tag>` backend chosen at load (e.g. `armv8.2_1`,
    /// `apple_m2_m3`). Omitted by single-backend builds.
    pub runtime_cpu_variant: Option<NonEmptyTrimmedString>,

    /// Version of the client build that ran this benchmark, not of the runtime
    /// ([`Self::runtime_version`]). Opaque grouping string: never parsed or
    /// ordered, so any versioning scheme works.
    pub client_version: Option<NonEmptyTrimmedString>,
    /// Milliseconds the device spent on the claimed job, on a monotonic clock
    /// from the claim until the result is ready. Stored as sent; no score uses
    /// it.
    pub device_time_ms: Option<ElapsedMillis>,
    /// A JSON object in a string, for whatever no other field names. Stored as
    /// sent and never read; see `docs/storage.md`.
    pub client_extras: Option<String>,
    /// Capped at ingest and stored apart from the body; see
    /// `record_submission` in mgmt.
    #[serde(flatten)]
    pub runtime_output: RuntimeOutput,

    /// The run's memory observation in bytes, on every benchmark type; what
    /// each figure counts is on [`crate::MemoryObservation`]. Distinct from
    /// [`Self::max_host_bytes`], the peak-memory benchmarks' metric: the two
    /// can differ on one run.
    pub observation_max_swap_bytes: Option<ByteCount>,
    pub observation_max_host_bytes: Option<ByteCount>,

    // Metrics: `validate` requires the ones the resolved `benchmark_type`
    // needs. Their types hold each one's range.
    pub prefill_time_ms: Option<PositiveMillis>,
    pub prefill_time_ms_stddev: Option<NonNegativeMillis>,
    pub decode_time_ms: Option<PositiveMillis>,
    pub decode_time_ms_stddev: Option<NonNegativeMillis>,
    pub total_time_ms: Option<PositiveMillis>,
    pub total_time_ms_stddev: Option<NonNegativeMillis>,
    /// Peak memory, also read as `max_ram_bytes` / `max_vram_bytes` (the Rust
    /// worker's spelling). A body with both spellings is refused up front
    /// (`reject_max_alias_collisions`), since an alias keeps one silently.
    #[serde(alias = "max_ram_bytes")]
    pub max_host_bytes: Option<ByteCount>,
    #[serde(alias = "max_vram_bytes")]
    pub max_gpu_bytes: Option<ByteCount>,
    pub max_npu_bytes: Option<ByteCount>,
    pub completions: Option<Vec<SampleCompletion>>,
    pub prompt_tokens: Option<PositiveCount>,
    /// An image always expands to at least one token, so absence is `None`
    /// and presence is a positive count.
    pub image_tokens: Option<PositiveCount>,
    pub prompt_ms: Option<PositiveMillis>,
    pub prompt_ms_stddev: Option<NonNegativeMillis>,
    pub predicted_ms: Option<PositiveMillis>,
    pub predicted_ms_stddev: Option<NonNegativeMillis>,

    // `asr_throughput`: vl_throughput's measurements, `asr_`-prefixed because the
    // client's result enum is untagged.
    pub asr_prompt_tokens: Option<PositiveCount>,
    pub asr_audio_tokens: Option<PositiveCount>,
    pub asr_prompt_ms: Option<PositiveMillis>,
    pub asr_prompt_ms_stddev: Option<NonNegativeMillis>,
    pub asr_predicted_ms: Option<PositiveMillis>,
    pub asr_predicted_ms_stddev: Option<NonNegativeMillis>,

    // `vl_spec_decode`: counters rather than rates, so a stored row can be
    // rescored under any convention the totals support.
    /// Turns the traversal actually covered.
    pub vl_spec_decode_turns: Option<PositiveCount>,
    /// Σ of the runtime's own prefill and generation timings. `predicted_ms` is
    /// the throughput denominator; with `prompt_ms` they give the reference
    /// harness's TTFT and end-to-end, comparable within one engine only.
    pub vl_spec_decode_prompt_ms: Option<NonNegativeMillis>,
    pub vl_spec_decode_predicted_ms: Option<PositiveMillis>,
    /// Σ over turns of the client's clock around the request, so queueing,
    /// upload and transport are in it.
    pub vl_spec_decode_ttft_ms: Option<NonNegativeMillis>,
    pub vl_spec_decode_decode_ms: Option<NonNegativeMillis>,
    /// The whole request, measured rather than `ttft + decode`.
    pub vl_spec_decode_e2e_ms: Option<PositiveMillis>,
    pub vl_spec_decode_generated_tokens: Option<i64>,
    /// The drafter's counters: present exactly when speculation was on, since
    /// zeros would put a `0 / 0` in the acceptance rate. Accepted includes
    /// drafts the target matched but the stop token discarded, so
    /// `generated − accepted` is not a forward count; it stays below generated
    /// only so the legacy `accept_length` division is defined.
    pub vl_spec_decode_drafted_tokens: Option<i64>,
    pub vl_spec_decode_accepted_tokens: Option<i64>,
    /// Σ over turns of each turn's `(generated − 1) / decode_forwards`, on the
    /// runtime's own decode forward count with the prefill out. A one-token
    /// reply ran no decode loop and adds 0. Over `turns`, the request-mean
    /// accept length the pooled counters cannot give.
    pub vl_spec_decode_turn_accept_length_sum: Option<f32>,

    // Not namespaced by benchmark type, so every type that serves a corpus
    // answers "which corpus did this row measure?" in one column.
    /// sha256 of the corpus tar the client downloaded and verified. The scorer
    /// compares it to the catalog's pin and records the verdict; it never
    /// rejects on a mismatch.
    pub observation_dataset_digest: Option<Sha256>,
    /// sha256 over a corpus traversal's replies, in order. Two rows over the
    /// same corpus with equal digests produced greedy-identical output.
    pub observation_output_digest: Option<Sha256>,
}

/// A `message_type: "failure"` submission: the benchmark, model and runtime
/// that could not be run, with its `failure_reason` and `retriable` flag.
#[derive(Debug, Deserialize, Serialize)]
pub struct FailureInput {
    pub benchmark_id: BenchmarkId,

    /// `true`: the failure is this client's, so the job stays open to others
    /// (this client is recorded as denied). `false`: it is inherent to the
    /// benchmark, model and runtime, so it ends the job. See `docs/queue.md`.
    pub retriable: bool,

    /// Why the benchmark could not be run. Typically includes a
    /// timestamp and the runtime's own error output.
    pub failure_reason: NonEmptyTrimmedString,

    pub model_name: Option<NonEmptyTrimmedString>,
    pub model_quant: Option<NonEmptyTrimmedString>,
    /// Full, lossless model specification; see [`SuccessInput::model_descriptor`].
    pub model_descriptor: Option<String>,
    pub model_flags: Option<NonEmptyTrimmedString>,
    pub runtime_name: Option<NonEmptyTrimmedString>,
    pub runtime_version: Option<NonEmptyTrimmedString>,
    /// Full, lossless runtime specification; see [`SuccessInput::runtime_descriptor`].
    pub runtime_descriptor: Option<String>,
    pub runtime_flags: Option<NonEmptyTrimmedString>,
    /// Version of the client build that failed to run this benchmark; see
    /// [`SuccessInput::client_version`].
    pub client_version: Option<NonEmptyTrimmedString>,
    /// The milliseconds from the claim to the moment the failure is ready to
    /// submit. See [`SuccessInput::device_time_ms`].
    pub device_time_ms: Option<ElapsedMillis>,
    /// See [`SuccessInput::client_extras`].
    pub client_extras: Option<String>,
    /// What the runtime printed before it failed; see
    /// [`SuccessInput::runtime_output`].
    #[serde(flatten)]
    pub runtime_output: RuntimeOutput,
}

/// One eval sample as a client submits it. Of the optional fields only
/// `stop_reason` reaches the scoring service: mgmt strips the rest before
/// `/score` (see `ScoreRequestSample`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SampleCompletion {
    /// Sample identifier, unique within a submission; keys the warehouse row.
    pub id: NonEmptyTrimmedString,
    /// `""` is a sample the client ran that produced no output, typically with
    /// `stop_reason: "failure"`.
    pub completion: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,
    /// Client-captured stop reason: `eos`, `truncated`, `doom_loop`, `failure`
    /// or `unknown` (see `docs/scoring-service.md`). A free string, not an
    /// enum, so a value new to a client lands without a mgmt release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// The raw signal behind `stop_reason`: the crash or transport detail for
    /// `failure`, the unclassified `stop_type` for `unknown`, the trigger for
    /// `doom_loop`; normally absent for `eos` / `truncated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_detail: Option<String>,
    /// Output tokens the client generated for this sample, which tell `eos`
    /// (< cap) from `truncated` (== cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
}

/// `POST /benchmarks`'s `202` receipt: the job the result was recorded under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobAccepted {
    pub job_id: String,
}

/// `POST /benchmarks/batch`'s answer, one entry per submission in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSubmitted {
    pub results: Vec<BatchItem>,
}

/// One batch entry: recorded under `job_id`, or refused with `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BatchItem {
    Accepted { index: usize, job_id: String },
    Rejected { index: usize, error: String },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::BenchmarkResultData;

    /// A payload as the Rust client builds it.
    fn payload() -> Result<BenchmarkSubmissionPayload, serde_json::Error> {
        serde_json::from_value(json!({
            "benchmark_id": "decode_throughput_256_128",
            "device_name": "bench box",
            "device_form_factor": "desktop",
            "device_os_name": "Linux",
            "device_os_version": "24.04",
            "device_chip_model": "x86_64",
            "device_ram_bytes": 34_359_738_368_u64,
            "model_descriptor": "{}",
            "runtime_descriptor": "{}",
            "submitted_at": "2026-10-08T00:00:00Z",
            "decode_time_ms": 50.0
        }))
    }

    /// A VL payload with the client's total.
    fn vl_payload() -> Result<BenchmarkSubmissionPayload, serde_json::Error> {
        serde_json::from_value(json!({
            "benchmark_id": "vl_throughput_384x384_32_64",
            "device_name": "bench box",
            "device_form_factor": "desktop",
            "device_os_name": "Linux",
            "device_os_version": "24.04",
            "device_chip_model": "x86_64",
            "device_ram_bytes": 34_359_738_368_u64,
            "model_descriptor": "{}",
            "runtime_descriptor": "{}",
            "submitted_at": "2026-10-08T00:00:00Z",
            "prompt_tokens": 75,
            "prompt_ms": 352.3,
            "predicted_ms": 32.7,
            "total_time_ms": 394.1,
            "total_time_ms_stddev": 4.2
        }))
    }

    /// The total decodes as `VlThroughput` and reaches the server's field.
    #[test]
    fn a_vl_total_reaches_the_server_under_its_name() -> Result<(), serde_json::Error> {
        let payload = vl_payload()?;
        assert!(
            matches!(
                payload.result,
                BenchmarkResultData::VlThroughput {
                    total_time_ms: Some(_),
                    total_time_ms_stddev: Some(_),
                    ..
                }
            ),
            "got {:?}",
            payload.result
        );
        let read = SuccessInput::try_from(&payload)?;
        assert_eq!(read.total_time_ms.map(f32::from), Some(394.1));
        assert_eq!(read.total_time_ms_stddev.map(f32::from), Some(4.2));
        assert!(read.prompt_tokens.is_some());
        Ok(())
    }

    /// The converted report is what the server parses from the payload's JSON,
    /// and the envelope carries the claimed job.
    #[test]
    fn a_payload_becomes_the_report_the_server_reads() -> Result<(), serde_json::Error> {
        let payload = payload()?;
        let read = SuccessInput::deserialize(serde_json::to_value(&payload)?)?;
        let converted = SuccessInput::try_from(&payload)?;
        assert_eq!(
            serde_json::to_value(&converted)?,
            serde_json::to_value(&read)?
        );

        let sent = serde_json::to_value(Submission::success(&payload, Some("job-1".into()))?)?;
        assert_eq!(sent.get("job_id"), Some(&json!("job-1")));
        assert_eq!(sent.get("message_type"), Some(&json!("success")));
        assert!(matches!(
            serde_json::from_value::<SubmissionInput>(sent)?,
            SubmissionInput::Success(_)
        ));
        Ok(())
    }

    /// A body without `message_type` is a success, as older clients send it.
    #[test]
    fn an_untagged_body_reads_as_a_success() -> Result<(), serde_json::Error> {
        let body = serde_json::to_value(SuccessInput::try_from(&payload()?)?)?;
        assert!(matches!(
            serde_json::from_value::<SubmissionInput>(body)?,
            SubmissionInput::Success(_)
        ));
        Ok(())
    }

    /// The server's refusal of a bad field is the conversion's.
    #[test]
    fn a_payload_the_server_refuses_does_not_convert() -> Result<(), serde_json::Error> {
        let mut payload = payload()?;
        payload.device.device_chip_model = "  ".into();
        assert!(SuccessInput::try_from(&payload).is_err());
        Ok(())
    }

    /// A batch entry is exactly one of the two shapes.
    #[test]
    fn a_batch_item_is_accepted_or_rejected() -> Result<(), serde_json::Error> {
        let answer: BatchSubmitted = serde_json::from_value(json!({"results": [
            {"index": 0, "job_id": "job-1"},
            {"index": 1, "error": "unknown benchmark"}
        ]}))?;
        assert_eq!(
            answer.results,
            [
                BatchItem::Accepted {
                    index: 0,
                    job_id: "job-1".into()
                },
                BatchItem::Rejected {
                    index: 1,
                    error: "unknown benchmark".into()
                },
            ]
        );
        Ok(())
    }
}
