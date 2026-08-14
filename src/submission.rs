//! Submission types — the wire schema for `POST /benchmarks` and the
//! storage schema for `submissions/incoming/` & `submissions/processed/`.
//!
//! Two layers:
//!
//! - [`SubmissionInput`] — what a client may send. The server-controlled
//!   identity fields `client_id`, `submitted_at`, and `benchmark_type` are
//!   absent from this type by design. `job_id` is the exception: a
//!   plan-attached run echoes the `job_id` it claimed (the handler peeks it
//!   from the raw body and validates it as a UUID), while ad-hoc/legacy
//!   clients omit it and the server mints a fresh UUID. Either way the
//!   resolved id is attached server-side, so `job_id` is not a field on the
//!   `*Input` structs.
//! - [`Submission`] — what lands on disk and what the scorer reads.
//!   Built from a [`SubmissionInput`] plus the server-side fields via
//!   [`SubmissionInput::into_submission`].
//!
//! Both layers are tagged on `message_type` (`"success"` | `"failure"`).
//! `Submission` uses `#[serde(flatten)]` to share the wire field list
//! with the corresponding `*Input` struct; renaming a wire field is
//! therefore a one-place edit.
//!
//! Domain validation lives on the input type:
//! [`SuccessInput::validate`] enforces rules that serde can't express
//! on the struct shape alone (form-factor enum, mill-params bounds,
//! GPU/NPU dependency rules, per-benchmark-type metric presence,
//! `Eval` completion-id uniqueness). The HTTP handler maps
//! [`ValidationError`] onto its own `AppError` — the rules themselves
//! know nothing about HTTP.

use std::collections::HashMap;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::benchmark::{Benchmark, BenchmarkDef, BenchmarkType};
use crate::scoring_service::SampleCompletion;
use crate::stores::SubmissionStore;
use crate::types::{BenchmarkId, ClientId, JobId};
use crate::validated::{BatteryLevel, NonEmptyTrimmedString};
use crate::warehouse::{
    AndroidTemperatureSensor, AndroidThermalStatus, AppleThermalState, DeviceFormFactor,
    DevicePowerState, LinuxThermalZone,
};

// ---------------------------------------------------------------------------
// Wire schema (SubmissionInput): what clients POST to /benchmarks
// ---------------------------------------------------------------------------

/// Wire-shape of a `POST /benchmarks` body. Tagged on `message_type`;
/// missing tag is defaulted to `"success"` by the handler before this
/// type is deserialized (back-compat for clients that predate the
/// failure variant).
#[derive(Debug, Deserialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
pub enum SubmissionInput {
    // Same boxing rationale as `Submission` below — keep the enum
    // compact to move around even though each variant carries a
    // large struct.
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

    /// Run domain-rule validation. `Failure` has no rules beyond
    /// what the struct shape enforces, so it's always `Ok`.
    pub fn validate(&self, benchmark: &Benchmark) -> Result<(), ValidationError> {
        match self {
            Self::Success(s) => s.validate(benchmark),
            // Failures have no metric rules, but their opaque refs must still
            // parse as JSON when present.
            Self::Failure(f) => {
                validate_json_ref("model_descriptor", f.model_descriptor.as_deref())?;
                validate_json_ref("runtime_descriptor", f.runtime_descriptor.as_deref())?;
                Ok(())
            }
        }
    }

    /// Attach server-side fields to produce the on-disk
    /// [`Submission`]. Consumes `self` because the input fields move
    /// into the inner `wire` slot.
    pub fn into_submission(
        self,
        client_id: ClientId,
        job_id: JobId,
        submitted_at: DateTime<Utc>,
        benchmark_type: BenchmarkType,
    ) -> Submission {
        match self {
            Self::Success(wire) => Submission::Success(Box::new(SuccessSubmission {
                wire: *wire,
                client_id,
                job_id,
                submitted_at,
                benchmark_type,
            })),
            Self::Failure(wire) => Submission::Failure(Box::new(FailureSubmission {
                wire: *wire,
                client_id,
                job_id,
                submitted_at,
                benchmark_type,
            })),
        }
    }
}

/// Wire fields for a `message_type: "success"` submission. Carries
/// the device / model / runtime context plus the per-`benchmark_type`
/// metric field(s); the handler resolves `benchmark_type` from the
/// catalog and attaches it server-side.
#[derive(Debug, Deserialize, Serialize)]
pub struct SuccessInput {
    pub benchmark_id: BenchmarkId,

    // Device info — required identity fields rejected when empty.
    pub device_name: NonEmptyTrimmedString,
    /// Required. The `parse::<DeviceFormFactor>()` step in `validate`
    /// owns enum-membership; `NonEmptyTrimmedString` guarantees the
    /// value is non-blank before that parse.
    pub device_form_factor: NonEmptyTrimmedString,
    pub device_os_name: NonEmptyTrimmedString,
    pub device_os_version: NonEmptyTrimmedString,
    /// Precise OS build identifier, finer-grained than `device_os_version`
    /// (e.g. iOS `22F76`, macOS `24F74`, Windows `26100.1234`, Android
    /// `AP3A.240905.015.A2`, Linux full `uname -r`). Optional — older clients
    /// omit it and it deserializes to `None`. Opaque grouping/display value.
    pub device_os_build: Option<NonEmptyTrimmedString>,
    /// OS security-patch level, where the platform exposes one. Currently
    /// Android-only (`Build.VERSION.SECURITY_PATCH`, e.g. `2025-06-01`); `None`
    /// on OSes that don't surface a distinct patch level. Optional.
    pub device_os_security_patch: Option<NonEmptyTrimmedString>,
    pub device_chip_model: NonEmptyTrimmedString,
    pub device_gpu_model: Option<NonEmptyTrimmedString>,
    pub device_gpu_vram_bytes: Option<i64>,
    pub device_npu_model: Option<NonEmptyTrimmedString>,
    pub device_npu_vram_bytes: Option<i64>,
    pub device_ram_bytes: i64,
    // Run-environment power state. Optional — older clients omit these and
    // they deserialize to `None` (like the GPU/NPU fields above).
    pub device_battery_level: Option<BatteryLevel>,
    pub device_power_state: Option<DevicePowerState>,
    pub device_power_save_mode: Option<bool>,
    // Per-platform per-iteration thermal telemetry. All optional — a client only
    // sends its own platform's fields and older clients omit them entirely, so
    // absent fields deserialize to `None` (like the power fields above). Sampled
    // around each measured repetition: `_before` at that rep's gate-pass and
    // `_after` once its timed work completes. The scalar families carry one
    // value per repetition; the sensor/zone families flatten every (iteration,
    // sensor) pair into one list, each element tagged with its `iteration` and a
    // plain `i32` °C accepted as reported. The worst condition over a run is
    // derivable from these series downstream, so it is not sent separately.
    pub device_apple_thermal_state_before: Option<Vec<AppleThermalState>>,
    pub device_apple_thermal_state_after: Option<Vec<AppleThermalState>>,
    /// Raw iOS SoC die temperature (fractional °C) sampled beside the Apple
    /// thermal-state enum above — same before/after split and per-repetition
    /// cardinality, but a numeric reading rather than a bucket. iOS-only and
    /// gated on the private-thermal client build (`PIPETTE_PRIVATE_THERMAL`).
    /// Only the whole array may be null/absent (sensor unreadable or flag off);
    /// per-element nulls are NOT accepted — `Option<Vec<f32>>` cannot
    /// deserialize a `[41.5, null, 44.0]`, so a client that can't read every
    /// repetition elides the whole series. Stored raw, with no rounding,
    /// bucketing, or delta.
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
    // Android CPU-scheduling diagnostics — the cpuset group and CPU affinity the
    // benchmark process actually ran under, so OEM demotion is visible (e.g.
    // Samsung placing a non-top-app service process in `/moderate`, off the prime
    // cores). Single-valued per submission (not per-repetition). All optional —
    // Android-only, and older clients omit them, so absent fields deserialize to
    // `None`. `_cpuset` is the cgroup path (`/top-app`, `/foreground`, …);
    // `_cpu_affinity_list` is a Linux CPU list (`0-5`, `0-3,6-7`);
    // `_cpu_affinity_excludes_top_tier` is true when the highest-frequency core
    // tier is not in the allowed set (the demotion signal).
    pub device_android_cpuset: Option<NonEmptyTrimmedString>,
    pub device_android_cpu_affinity_list: Option<NonEmptyTrimmedString>,
    pub device_android_cpu_affinity_excludes_top_tier: Option<bool>,

    // Model info
    /// Human-facing model identity / grouping key (e.g. `{repo}:{filename}`).
    /// Optional — a submission may instead carry only the lossless
    /// [`Self::model_descriptor`], or both. Warehouse grouping and display key off
    /// this when present.
    pub model_name: Option<NonEmptyTrimmedString>,
    /// Convenience quantization label for the primary artifact. Optional and
    /// lossy for multi-artifact models — the authoritative per-piece quant
    /// lives inside `model_descriptor`.
    pub model_quant: Option<NonEmptyTrimmedString>,
    /// Total parameter count in millions. Optional at the HTTP
    /// boundary; the scorer fills this from the catalog when known.
    pub model_params_total_millions: Option<i32>,
    /// Active parameter count (drives prefill/decode throughput).
    /// Optional; for dense models defaults to total; for MoE this is
    /// set explicitly. Validation requires `active <= total` when
    /// both are present.
    pub model_params_active_millions: Option<i32>,
    /// Full, lossless model specification — the client's typed model
    /// descriptor serialized to a **JSON string** (one artifact for an MLX
    /// bundle, several for a llama.cpp VL or audio model). Opaque to the
    /// server: it is never deserialized into a known type (partners define
    /// their own runtimes and formats), only parsed to canonicalize it (keys
    /// sorted, whitespace stripped) and stored, so pattern search over it is
    /// stable. See [`crate::canonical_json`].
    pub model_descriptor: Option<String>,
    /// Opaque configuration affecting model behavior. Typically a JSON string
    /// going forward (e.g. `{"enable_thinking":true}`), but a plain string is
    /// equally valid — the server never validates or interprets it, storing it
    /// verbatim as the cheap grouping/display field.
    pub model_flags: Option<NonEmptyTrimmedString>,

    // Runtime info
    /// Cheap grouping/display runtime name. Optional — the authoritative
    /// identity (name, version, build) lives in [`Self::runtime_descriptor`].
    pub runtime_name: Option<NonEmptyTrimmedString>,
    /// Cheap grouping/display version string. Optional — the authoritative
    /// version is baked into [`Self::runtime_descriptor`] when present.
    pub runtime_version: Option<NonEmptyTrimmedString>,
    /// Full, lossless runtime specification — the client's typed `Runtime`
    /// descriptor serialized to a **JSON string**, with the version/build
    /// coordinates baked in. Opaque to the server (canonicalized and stored,
    /// never deserialized); `runtime_name` / `runtime_version` stay separate as
    /// the cheap grouping/display fields. See [`crate::canonical_json`].
    pub runtime_descriptor: Option<String>,
    /// Opaque configuration affecting the runtime itself. Typically a JSON
    /// string going forward, but a plain string (e.g. `--n-gpu-layers 999`) is
    /// equally valid — the server never validates or interprets it, storing it
    /// verbatim as the cheap grouping/display field.
    pub runtime_flags: Option<NonEmptyTrimmedString>,
    /// The **resolved** harness configuration the run executed under —
    /// readiness gating, timeouts, loop detection — as a JSON string. Opaque
    /// to the server (canonicalized and stored with a `_sha256` alongside,
    /// never deserialized), like the descriptors rather than like
    /// [`Self::runtime_flags`]: it is a grouping key for "runs measured the
    /// same way", so pattern search over it has to be stable.
    ///
    /// Resolved, not authored — a client that left a setting unset submits the
    /// value it ran with, never null. The server cannot check that; see
    /// `docs/storage.md` § `benchmark_flags`.
    pub benchmark_flags: Option<String>,
    /// Runtime-selected CPU kernel variant, interpreted per
    /// `runtime_name`. For llama.cpp/ggml: the `ggml-cpu-<tag>` backend
    /// variant chosen at load time by feature-dispatch scoring (e.g.
    /// `armv8.2_1`, `android_armv8.6_1`, `apple_m2_m3`). Optional —
    /// omitted by single-static-backend builds (no runtime dispatch).
    pub runtime_cpu_variant: Option<NonEmptyTrimmedString>,

    /// Version of the client build that ran and submitted this benchmark —
    /// the harness, not the inference runtime it drove ([`Self::runtime_version`]).
    /// Cheap grouping/display string, opaque to the server: it never parses or
    /// orders it, so any versioning scheme a client uses is fine. Optional —
    /// older clients omit it and it deserializes to `None`.
    pub client_version: Option<NonEmptyTrimmedString>,

    /// Peak swap and host memory that the run held, in bytes. Every benchmark
    /// reports them, so the scorer gates neither on `benchmark_type`. The swap
    /// term is contained in the host peak rather than additional to it.
    ///
    /// Distinct from [`Self::max_host_bytes`], the measurement that the
    /// peak-memory benchmarks require, which becomes a `max_host_usage`
    /// metric. These count compressed and paged-out memory where the platform
    /// exposes it, so the two disagree by design on a peak-memory run.
    ///
    /// The client sends each observation under its warehouse column name.
    /// Optional: a client that does not sample memory omits them, and they
    /// deserialize to `None`.
    pub observation_max_swap_bytes: Option<i64>,
    pub observation_max_host_bytes: Option<i64>,

    // Per-benchmark-type metrics — all optional on the struct;
    // `validate` enforces the right field is present given the
    // resolved `benchmark_type`.
    pub prefill_time_ms: Option<f32>,
    pub prefill_time_ms_stddev: Option<f32>,
    pub decode_time_ms: Option<f32>,
    pub decode_time_ms_stddev: Option<f32>,
    pub total_time_ms: Option<f32>,
    pub total_time_ms_stddev: Option<f32>,
    /// Memory: legacy clients send `max_ram_bytes` / `max_vram_bytes`;
    /// both names map to the new fields via `#[serde(alias)]`. Aliased
    /// keys silently overwrite on collision — the handler rejects
    /// both-present up front (see `reject_max_alias_collisions`).
    #[serde(alias = "max_ram_bytes")]
    pub max_host_bytes: Option<i64>,
    #[serde(alias = "max_vram_bytes")]
    pub max_gpu_bytes: Option<i64>,
    pub max_npu_bytes: Option<i64>,
    pub completions: Option<Vec<SampleCompletion>>,
    pub prompt_tokens: Option<i64>,
    /// An image always expands to at least one token, so absence is `None`
    /// and presence is a positive count — encoded with `NonZeroI64` so a
    /// stray `0` is rejected at deserialization rather than silently scored.
    pub image_tokens: Option<std::num::NonZeroI64>,
    pub prompt_ms: Option<f32>,
    pub prompt_ms_stddev: Option<f32>,
    pub predicted_ms: Option<f32>,
    pub predicted_ms_stddev: Option<f32>,
}

impl SuccessInput {
    /// Post-deserialize rules that serde can't express on the struct
    /// shape: form-factor enum parse, mill-params positivity +
    /// ordering, observed-byte-count non-negativity, GPU/NPU
    /// presence-of-X-implies-Y, per-`benchmark_type` metric field
    /// presence, and completion-id uniqueness on `Eval`.
    pub fn validate(&self, benchmark: &Benchmark) -> Result<(), ValidationError> {
        // `TrimmedString` already stripped whitespace, so parse
        // directly — `"  embedded\n"` from a shell-pipe client
        // reaches here as `"embedded"`.
        self.device_form_factor
            .parse::<DeviceFormFactor>()
            .map_err(|e| ValidationError::FormFactor(e.to_string()))?;

        if let Some(t) = self.model_params_total_millions
            && t <= 0
        {
            return Err(ValidationError::NonPositiveMillParams(
                "model_params_total_millions",
            ));
        }
        if let Some(a) = self.model_params_active_millions
            && a <= 0
        {
            return Err(ValidationError::NonPositiveMillParams(
                "model_params_active_millions",
            ));
        }
        if let (Some(t), Some(a)) = (
            self.model_params_total_millions,
            self.model_params_active_millions,
        ) && a > t
        {
            return Err(ValidationError::ActiveExceedsTotal {
                active: a,
                total: t,
            });
        }

        // A byte count is a peak, so `0` is a real reading — a run that
        // touched no swap reports `0`, not `null`. Only a negative value is
        // impossible, and it would reach the warehouse as a nonsense peak and
        // skew every aggregate over the column.
        if let Some((field, _)) = [
            (
                "observation_max_swap_bytes",
                self.observation_max_swap_bytes,
            ),
            (
                "observation_max_host_bytes",
                self.observation_max_host_bytes,
            ),
        ]
        .into_iter()
        .find(|(_, value)| value.is_some_and(|v| v < 0))
        {
            return Err(ValidationError::NegativeBytes(field));
        }

        if self.device_gpu_vram_bytes.is_some() && self.device_gpu_model.is_none() {
            return Err(ValidationError::GpuVramRequiresGpuModel);
        }
        if self.device_npu_vram_bytes.is_some() && self.device_npu_model.is_none() {
            return Err(ValidationError::NpuVramRequiresNpuModel);
        }
        // `device_battery_level`'s 0–100 range is enforced by the
        // `BatteryLevel` type at `Deserialize` time, so there's no range
        // check here — an out-of-range reading is already a 400 before
        // `validate` runs.

        match &benchmark.def {
            BenchmarkDef::PrefillThroughput { .. } => {
                if self.prefill_time_ms.is_none() {
                    return Err(ValidationError::MissingMetric("prefill_time_ms"));
                }
            }
            BenchmarkDef::DecodeThroughput { .. } => {
                if self.decode_time_ms.is_none() {
                    return Err(ValidationError::MissingMetric("decode_time_ms"));
                }
            }
            BenchmarkDef::EndToEndLatency { .. } => {
                if self.total_time_ms.is_none() {
                    return Err(ValidationError::MissingMetric("total_time_ms"));
                }
            }
            BenchmarkDef::MaxMemoryUsage { .. } | BenchmarkDef::VlMaxMemory { .. } => {
                if self.max_host_bytes.is_none() {
                    return Err(ValidationError::MissingMetric("max_host_bytes"));
                }
            }
            BenchmarkDef::Eval { .. } => {
                let completions = self
                    .completions
                    .as_ref()
                    .ok_or(ValidationError::MissingMetric("completions"))?;
                // The scoring service also rejects duplicate completion
                // ids, but only at cron time — catching them here keeps
                // a bad submission from parking in `incoming/` until
                // manually cleared.
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
            BenchmarkDef::VlThroughput { .. } => {
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
        }

        // `model_descriptor` / `runtime_descriptor` are opaque to the server, but if present
        // they must at least parse as JSON — the scorer parses them to
        // canonicalize, and a non-JSON blob would silently defeat that.
        validate_json_ref("model_descriptor", self.model_descriptor.as_deref())?;
        validate_json_ref("runtime_descriptor", self.runtime_descriptor.as_deref())?;
        validate_json_object("benchmark_flags", self.benchmark_flags.as_deref())?;

        Ok(())
    }
}

/// Reject a `model_descriptor` / `runtime_descriptor` that is present but not valid JSON.
/// The value is otherwise opaque; this only guarantees the scorer can parse it
/// to canonicalize. Absent (`None`) is fine.
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

/// Reject a `benchmark_flags` that is present but not a valid JSON **object**.
///
/// Stricter than [`validate_json_ref`]: this field is always a map of settings,
/// and a bare string or array would canonicalize fine and then be useless as a
/// grouping key.
///
/// This is the *only* thing standing between a non-object and the warehouse —
/// [`crate::canonical_json::canonicalize_str`] deliberately passes unparseable
/// input through unchanged, so a path that reaches the scorer without going
/// through here (a submission written straight to disk) has nothing else
/// checking the shape.
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

/// Wire fields for a `message_type: "failure"` submission. Carries
/// the identity of the (benchmark, model, runtime) tuple that couldn't
/// be executed — keyed by the server-side `job_id` — plus the
/// human-readable `failure_reason` and the `retriable` routing flag.
#[derive(Debug, Deserialize, Serialize)]
pub struct FailureInput {
    pub benchmark_id: BenchmarkId,

    /// Required (`httpapi.md §2.7.2`). `true` — the failure is specific to
    /// this client, so the job stays available to others (a `denied/` marker
    /// is recorded for this client). `false` — the failure is inherent to the
    /// benchmark/model/runtime, so the job is torn down as its terminal
    /// result. Drives the routing in `planner.md §Consequences of Failure`.
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
    /// [`SuccessInput::client_version`]. Kept on this variant too because
    /// "which client build reports this failure" is exactly the question a
    /// failure raises.
    pub client_version: Option<NonEmptyTrimmedString>,
}

// ---------------------------------------------------------------------------
// Storage schema (Submission): what's on disk and what the scorer reads
// ---------------------------------------------------------------------------

/// On-disk submission body — what's persisted under `submissions/`
/// and what the scorer dispatches on. Tagged on `message_type`; both
/// variants include the server-injected identity fields
/// (`client_id`, `job_id`, `submitted_at`, `benchmark_type`) and
/// flatten the matching `*Input` for the wire-supplied fields.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
pub enum Submission {
    // Both variants are large (`SuccessSubmission` is ~670 bytes;
    // `FailureSubmission` is ~300). Box them so `Submission` itself
    // stays small to move through `match` returns and
    // `Result<ScoreOutcome>`. Without boxing, clippy's
    // `large_enum_variant` flags the >300-byte stack footprint of
    // the smallest variant.
    Success(Box<SuccessSubmission>),
    Failure(Box<FailureSubmission>),
}

/// Storage shape of a successful submission. Read by the scorer from
/// `incoming/{job_id}.json`. Wire-supplied fields live on `wire`;
/// server-injected fields are top-level.
#[derive(Debug, Deserialize, Serialize)]
pub struct SuccessSubmission {
    #[serde(flatten)]
    pub wire: SuccessInput,
    pub client_id: ClientId,
    pub job_id: JobId,
    pub submitted_at: DateTime<Utc>,
    pub benchmark_type: BenchmarkType,
}

/// Storage shape of a failure submission. Written directly to
/// `processed/` by the handler; `GET /jobs/{job_id}` surfaces
/// `status: "failed"` plus `failure_reason`.
#[derive(Debug, Deserialize, Serialize)]
pub struct FailureSubmission {
    #[serde(flatten)]
    pub wire: FailureInput,
    pub client_id: ClientId,
    pub job_id: JobId,
    pub submitted_at: DateTime<Utc>,
    pub benchmark_type: BenchmarkType,
}

// ---------------------------------------------------------------------------
// Domain-level validation errors
// ---------------------------------------------------------------------------

/// Domain-level submission validation failures. Independent of the
/// HTTP layer; the handler maps these onto `AppError::BadRequest`
/// using their `Display` form.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("{0}")]
    FormFactor(String),
    #[error("{0} must be a positive integer")]
    NonPositiveMillParams(&'static str),
    #[error("{0} must not be negative")]
    NegativeBytes(&'static str),
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
    #[error(
        "duplicate completion id at completions[{index}]: {id:?} \
         (first seen at completions[{first_seen}])"
    )]
    DuplicateCompletionId {
        index: usize,
        id: String,
        first_seen: usize,
    },
    /// Both the new and legacy name of an aliased memory field are
    /// present on the body. Pre-deserialize raw-body check;
    /// `#[serde(alias)]` silently keeps one side on collision.
    #[error("{new} and legacy {legacy} both present; send only one")]
    AliasCollision {
        new: &'static str,
        legacy: &'static str,
    },
}

/// Pre-deserialize raw-body inspection for the `max_*_bytes` alias
/// pairs. `#[serde(alias)]` silently keeps one side on collision —
/// to reject both-present we have to look at the un-deserialized
/// `serde_json::Value` before the field merge happens. Only relevant
/// for `MaxMemoryUsage` benchmarks; harmless on others (the fields
/// aren't read).
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

/// A `spec.model` / `spec.runtime` object rendered as the wire descriptor form:
/// a canonical JSON *string*, with every `auth_token` dropped.
///
/// A plan may carry the access token for a gated model repository inside its
/// model spec, and a synthetic failure record is stored and queryable like any
/// other — so the token never reaches the warehouse. The claim response is
/// unaffected: a client needs the token to fetch the repo, so it travels there
/// deliberately.
fn descriptor_from_spec(spec_field: &serde_json::Value) -> String {
    crate::canonical_json::canonicalize(&without_auth_tokens(spec_field))
}

/// `value` with every object entry keyed `auth_token` **removed**, recursively.
///
/// Removed rather than replaced with a redaction marker, to match how a client
/// spells the same model: clients serialize through `Model::without_auth_token()`,
/// which omits the key entirely, so a marker would leave the two differing on
/// precisely the gated models.
///
/// This narrows a divergence; it does not close it. A client descriptor is a
/// *typed* round-trip and drops any field its schema does not describe, while this
/// canonicalizes the raw JSON and keeps everything — so the two agree only for
/// specs carrying no fields beyond the client's schema. Nothing may join the two
/// sources on descriptor equality; see `docs/plan-ingestion.md` §9.
fn without_auth_tokens(value: &serde_json::Value) -> serde_json::Value {
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
        leaf => leaf.clone(),
    }
}

/// A job body from which no terminal failure record can ever be built.
///
/// Typed, rather than a message, because the **disposition** differs and a caller
/// has to act on the distinction. An ordinary failure here — a store write, a
/// catalog not yet loaded — is worth retrying on the next `queue-maintenance`
/// run. This is not: the body never changes, so every retry fails identically,
/// and the entry would sit in `avail/` being re-warned about forever. The caller
/// deletes it instead ([`crate::queue_maintenance`]).
///
/// Deliberately **not** raised for a `benchmark_id` the catalog does not know.
/// That is operator-restorable — putting the definition back makes the same body
/// recordable — so it stays retriable, and its error text says as much.
///
/// A body this broken cannot belong to a plan: ingestion refuses one lacking
/// `job_id` or `spec.benchmark` (`plan_ingestion::validate_job`), so no manifest
/// lists it and deleting it cannot stall a plan's completion.
#[derive(Debug, thiserror::Error)]
#[error("job body cannot produce a failure record: {0}")]
pub struct UnrecordableJob(String);

/// Build a synthetic **terminal** failure submission attributed to the reserved
/// `"system"` client, sourced from a `todo/` job body. The server writes one
/// when it declares a job failed without a client run producing the result:
/// every eligible client denied a `clients`-only job, or the job's `expires_at`
/// passed (see `planner.md`, "Consequences of Failure"). The
/// `benchmark_id` and the descriptors are derived from the job body's `spec` —
/// the authoritative description of the work — and `retriable` is always `false`
/// (a system-declared failure is terminal, so it routes through the normal
/// failure pipeline).
///
/// A body missing or malforming a field the record requires yields
/// [`UnrecordableJob`], which the caller treats as permanent rather than
/// retrying it.
///
/// The scalar `model_*` / `runtime_*` grouping labels are left unset. Recovering
/// them would mean parsing the partner-defined model and runtime schemas inside
/// `spec`, which this server deliberately cannot do (see [`crate::canonical_json`]);
/// the descriptors carry the same information losslessly, and the scorer already
/// falls back to matching against them when `model_name` is absent
/// ([`crate::score`]).
pub fn system_failure_from_job_body(
    job_body: &serde_json::Value,
    benchmark_type: BenchmarkType,
    failure_reason: impl Into<String>,
    submitted_at: DateTime<Utc>,
) -> anyhow::Result<Submission> {
    let job_id = job_body
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| UnrecordableJob("missing required field `job_id`".into()))?;
    let spec = job_body
        .get("spec")
        .ok_or_else(|| UnrecordableJob("missing required field `spec`".into()))?;
    let benchmark_id = BenchmarkId::try_new(
        spec.get("benchmark")
            .and_then(|v| v.as_str())
            .ok_or_else(|| UnrecordableJob("`spec` missing required field `benchmark`".into()))?,
    )
    .map_err(|e| UnrecordableJob(format!("invalid `spec.benchmark`: {e}")))?;

    let wire = FailureInput {
        benchmark_id,
        retriable: false,
        failure_reason: NonEmptyTrimmedString::try_new(failure_reason.into())?,
        model_name: None,
        model_quant: None,
        // `filter` before `map`, because `Value::get` returns `Some(Value::Null)`
        // for a present-but-null key: without it a null model canonicalizes to the
        // literal string `"null"`, which is worse than an absent descriptor. It
        // would be stored, hashed, and grouped in the warehouse as if it named a
        // real cell, so every null-model record would share one meaningless
        // `model_descriptor_sha256`.
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
        // No client ran this: the failure is declared by the server from the
        // job body, so there is no client build to name.
        client_version: None,
    };

    Ok(Submission::Failure(Box::new(FailureSubmission {
        wire,
        client_id: ClientId::try_new("system")?,
        job_id: JobId::try_new(job_id)
            .map_err(|e| UnrecordableJob(format!("invalid `job_id`: {e}")))?,
        submitted_at,
        benchmark_type,
    })))
}

/// Dispatch a submission's on-disk write target on whether it is held and on
/// the submission variant.
///
/// - **Held** (pending client) bodies land in the write-only
///   `unverified/{client_id}/` archive and never enter the scorer until
///   an operator promotes them (`pipette-mgmt unverified promote`).
/// - Pipeline **`Success`** bodies land in `incoming/` for the scorer;
///   pipeline **`Failure`** bodies are already terminal and go straight
///   to `processed/`. Without this split, failures would sit in
///   `incoming/` indefinitely and `GET /jobs/{job_id}` would lie about
///   the job being pending.
pub async fn write_submission_record(
    store: &dyn SubmissionStore,
    submission: &Submission,
    body: &serde_json::Value,
    held: bool,
) -> anyhow::Result<()> {
    let (job_id, client_id) = match submission {
        Submission::Success(s) => (&s.job_id, &s.client_id),
        Submission::Failure(f) => (&f.job_id, &f.client_id),
    };
    if held {
        store
            .write_unverified(client_id, job_id, body)
            .await
            .context("failed to write unverified submission")?;
        tracing::info!(job_id = %job_id, client_id = %client_id, "submission held in unverified");
        return Ok(());
    }
    match submission {
        Submission::Success(_) => {
            store
                .write_incoming(job_id, body)
                .await
                .context("failed to write submission")?;
            tracing::info!(job_id = %job_id, "submission written to incoming");
        }
        Submission::Failure(_) => {
            store
                .write_processed(job_id, body)
                .await
                .context("failed to write failure submission")?;
            tracing::info!(job_id = %job_id, "failure submission written to processed");
        }
    }
    Ok(())
}

/// Record a synthetic `"system"` failure for a `todo/` job: resolve the
/// benchmark type from the catalog, build the terminal failure submission
/// ([`system_failure_from_job_body`]), and write it. The record is routed by
/// [`write_submission_record`] straight to `processed/` — the scorer has
/// nothing to compute for a failure, so an `incoming/` write would linger
/// forever — and is never archived as unverified (`held = false`; no client
/// produced it). Both server-declared failure paths (the all-clients-denied
/// escalation and `queue-maintenance`'s expiry pass) go through here; each
/// caller owns its surrounding teardown and error policy.
pub async fn record_system_failure(
    store: &dyn SubmissionStore,
    catalog: &HashMap<BenchmarkId, Benchmark>,
    job_body: &serde_json::Value,
    failure_reason: impl Into<String>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    // Two failures live here with opposite dispositions, so they are raised
    // separately rather than collapsed into one `and_then` chain.
    //
    // A body that names no benchmark, or names an unusable one, is
    // [`UnrecordableJob`]: no catalog change can make it recordable, so the caller
    // drops it instead of retrying forever.
    let benchmark_id = job_body
        .get("spec")
        .and_then(|spec| spec.get("benchmark"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| UnrecordableJob("`spec.benchmark` is absent or not a string".into()))?;
    let benchmark_id = BenchmarkId::try_new(benchmark_id)
        .map_err(|e| UnrecordableJob(format!("invalid `spec.benchmark`: {e}")))?;
    // A benchmark the catalog does not know is the *other* case: restoring the
    // definition makes this same body recordable, so it stays retriable and the
    // message carries the remediation. Retried every run until an operator acts —
    // deliberately, so the misconfiguration surfaces through cron monitoring.
    let benchmark_type = catalog
        .get(&benchmark_id)
        .map(|b| b.benchmark_type())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot resolve benchmark_type for synthetic failure (spec.benchmark: \
                 {benchmark_id}); restore the benchmark definition or delete the job from \
                 todo/avail/"
            )
        })?;
    let synthetic = system_failure_from_job_body(job_body, benchmark_type, failure_reason, now)?;
    let body = serde_json::to_value(&synthetic)?;
    write_submission_record(store, &synthetic, &body, false).await
}

/// Parse an on-disk submission body, defaulting `message_type` to
/// `"success"` when absent. The HTTP handler always injects
/// `message_type` on new writes, but legacy bodies written before
/// the enum landed (≈20k objects in production) lack the field.
/// Running the `fix-message-type` migration to backfill them was
/// decided against on cost grounds, so the read path tolerates the
/// absence instead.
///
/// This is the *only* tolerated drift between wire/storage schema
/// and the on-disk reality. Every other field requirement is
/// enforced by serde via the `Submission` enum.
pub fn parse_stored_submission(body: &serde_json::Value) -> Result<Submission, serde_json::Error> {
    if body.get("message_type").is_some() {
        // Already self-describing — parse as-is, no allocation.
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
mod tests {
    use super::*;
    use crate::warehouse::AndroidThrottlingSeverity;
    use rstest::rstest;
    use serde_json::json;

    /// A body without `message_type` parses as `Success` — the fallback
    /// for ~20k legacy bodies in production whose tag wasn't backfilled.
    #[test]
    fn parse_stored_submission_defaults_missing_tag_to_success() {
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
        let parsed = parse_stored_submission(&body).expect("should parse");
        assert!(matches!(parsed, Submission::Success(_)));
    }

    /// The optional `device_os_build` / `device_os_security_patch` fields parse
    /// when present and default to `None` when absent, so new and old clients
    /// both submit successfully.
    #[test]
    fn parse_success_reads_optional_os_build_and_security_patch() {
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
            "device_chip_model": "chip",
            "device_ram_bytes": 16_000_000_000_i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0,
        });
        let Submission::Success(s) = parse_stored_submission(&with_fields).expect("should parse")
        else {
            panic!("expected success");
        };
        assert_eq!(
            s.wire.device_os_build.clone().map(String::from),
            Some("AP3A.240905.015.A2".to_string()),
        );
        assert_eq!(
            s.wire.device_os_security_patch.clone().map(String::from),
            Some("2025-06-01".to_string()),
        );

        // Absent → None: submissions from clients that predate the fields.
        let mut without = with_fields;
        let obj = without.as_object_mut().expect("object");
        obj.remove("device_os_build");
        obj.remove("device_os_security_patch");
        let Submission::Success(s) = parse_stored_submission(&without).expect("should parse")
        else {
            panic!("expected success");
        };
        assert_eq!(s.wire.device_os_build, None);
        assert_eq!(s.wire.device_os_security_patch, None);
    }

    /// An explicit `message_type` is honored over the default. A stray
    /// legacy `plan_id` is silently ignored (no `deny_unknown_fields`),
    /// per the unknown-fields compatibility guarantee in `httpapi.md §2.7`.
    #[test]
    fn parse_stored_submission_honors_explicit_failure_tag() {
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
        let parsed = parse_stored_submission(&body).expect("should parse");
        assert!(matches!(parsed, Submission::Failure(_)));
    }

    /// A job envelope carrying `spec` with the given `model` and `runtime`.
    ///
    /// Only those two vary across the descriptor tests below; `job_id` and
    /// `spec.benchmark` are the scaffolding every one of them needs and none of
    /// them is asserting on.
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

    /// A synthetic system failure takes its identity from the job envelope and
    /// its descriptors from `spec`, is attributed to the reserved `"system"`
    /// client, and is always non-retriable (terminal). The descriptors are stored
    /// canonically (keys sorted, compact) — the same normalization a
    /// client-submitted descriptor gets, though not a guarantee the two are equal
    /// (see `without_auth_tokens`).
    #[test]
    fn system_failure_from_job_body_derives_descriptors_from_the_spec() -> anyhow::Result<()> {
        let job_body = json!({
            "job_id": "550e8400-e29b-41d4-a716-446655440000",
            "expires_at": "never",
            "clients": ["one", "two"],
            "spec": {
                "benchmark": "prefill_throughput_256",
                // Deliberately not in sorted key order — canonicalization sorts.
                "model": {"type": "gguf_text", "source": "huggingface", "org": "o", "repo_name": "r", "path": "m.gguf"},
                "runtime": {"type": "llamacpp_cli_stock_tools", "flavor": "macos-arm64"},
            },
        });
        let at = "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?;
        let submission = system_failure_from_job_body(
            &job_body,
            BenchmarkType::PrefillThroughput,
            "All eligible clients reported failure",
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

    /// A present-but-null `model` / `runtime` yields **no** descriptor, not the
    /// canonicalized string `"null"`.
    ///
    /// `Value::get` cannot tell an absent key from a null one, so the null has to
    /// be filtered explicitly. Getting this wrong is silent and durable: `"null"`
    /// would be stored and hashed like any other descriptor, so every such record
    /// would group under one meaningless `model_descriptor_sha256` in the
    /// warehouse. Ingestion rejects a null model, but this function is documented
    /// best-effort against job bodies written straight into `avail/`.
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

    /// A gated-repo access token in the plan's model spec must not reach a stored
    /// failure record. The claim response still carries it — the client needs it
    /// to fetch the repo — so the stripping belongs here, not on the way out.
    #[test]
    fn system_failure_redacts_an_auth_token_from_the_spec() -> anyhow::Result<()> {
        // A token is planted at all three depths the stripping has to reach. The
        // nested and in-array positions are not hypothetical: a model can be
        // several artifacts (a VL backbone plus its projector; an audio model with
        // backbone, encoder-projector, vocoder, and tokenizer — see
        // `docs/storage.md`), which is exactly where a token sits below the top
        // level. If the recursion regressed, a credential would be written to the
        // warehouse, and nothing downstream would ever reveal it.
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
            "2026-06-29T00:00:00Z".parse::<DateTime<Utc>>()?,
        )?;
        let Submission::Failure(f) = submission else {
            anyhow::bail!("expected a failure submission");
        };

        // Checked against the whole serialized record, not just the descriptor: a
        // leak anywhere in the stored body is a leak.
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
        // The key is gone, not marked, matching how a client spells the same
        // model — it omits the key entirely.
        assert!(
            !descriptor.contains("auth_token"),
            "a redaction marker would break descriptor identity: {descriptor}"
        );
        // Everything that is not a token survives, at every depth.
        assert_eq!(
            descriptor,
            r#"{"backbone":{"path":"model.gguf"},"extra_files":[{"path":"mmproj.gguf"}],"org":"o","repo_name":"r","source":"huggingface","type":"gguf_vision"}"#
        );
        Ok(())
    }

    /// A job body missing a field the failure record requires is an error, not
    /// a record with empty strings. The grouping labels and descriptors are all
    /// optional; `job_id` and the `spec` carrying `benchmark` are what still
    /// trigger the error when absent.
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
        assert!(system_failure_from_job_body(
            &job_body,
            BenchmarkType::PrefillThroughput,
            "reason",
            at,
        )
        .is_err());
        Ok(())
    }

    /// The run-environment power fields and `runtime_cpu_variant` are
    /// optional on the wire: a body that omits them deserializes with all
    /// four as `None` (no `#[serde(default)]` needed — serde treats a
    /// missing `Option` field as `None`). Locks optionality in against a
    /// future `deny_unknown_fields` or a refactor that drops the `Option`.
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
        // The per-platform thermal fields are optional too — an omitting body
        // deserializes all ten (before/after per family) to `None`.
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
        // The Android CPU-scheduling diagnostics are optional too.
        assert_eq!(success.wire.device_android_cpuset, None);
        assert_eq!(success.wire.device_android_cpu_affinity_list, None);
        assert_eq!(
            success.wire.device_android_cpu_affinity_excludes_top_tier,
            None
        );
        // The per-run memory observations are optional too.
        assert_eq!(success.wire.observation_max_swap_bytes, None);
        assert_eq!(success.wire.observation_max_host_bytes, None);
        Ok(())
    }

    /// A body carrying the per-run memory observations deserializes both as
    /// `i64`, including a value above `2^32` — the swap and host peaks of a
    /// large run exceed what an `i32` holds. Every benchmark type reports them,
    /// so a `prefill_throughput` body carries them here.
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
        assert_eq!(success.wire.observation_max_swap_bytes, Some(2_147_483_648));
        assert_eq!(success.wire.observation_max_host_bytes, Some(5_368_709_120));
        // The peak-memory benchmarks' own measurement is a separate field and
        // stays absent.
        assert_eq!(success.wire.max_host_bytes, None);
        Ok(())
    }

    /// A body carrying the Android CPU-scheduling diagnostics deserializes them:
    /// the cpuset path + affinity list as non-empty strings and the top-tier
    /// exclusion flag as a bool. Complements the omit-all case above.
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
            success.wire.device_android_cpuset.as_deref(),
            Some("/moderate")
        );
        assert_eq!(
            success.wire.device_android_cpu_affinity_list.as_deref(),
            Some("0-5")
        );
        assert_eq!(
            success.wire.device_android_cpu_affinity_excludes_top_tier,
            Some(true)
        );
        Ok(())
    }

    /// A body carrying the per-platform thermal fields deserializes them:
    /// the Apple state enum, the Android status/headroom, an Android sensor
    /// array, and a Linux zone array — the array elements' `type`/`name`/
    /// `celsius`/`throttling_status` all survive, `celsius` as a plain `i32`.
    /// Complements the omit-all case above.
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

        // The sensor list flattens every (iteration, sensor) pair; `iteration`
        // tags which repetition each element belongs to.
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
}
