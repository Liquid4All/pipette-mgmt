use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, BooleanArray, Int32Array, Int64Array, new_null_array};
use arrow::array::{Float32Array, StringArray, TimestampMicrosecondArray};
use arrow::array::{ListArray, StructArray};
use arrow::buffer::{NullBuffer, OffsetBuffer};
use arrow::datatypes::{DataType, Field, Fields, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use strum::{AsRefStr, Display, EnumIter, EnumString};

use crate::benchmark::BenchmarkType;
use crate::parquet_utils::{
    WriterOpts, read_batches_from_bytes, read_batches_from_file, write_batch_bytes,
    write_batches_to_file,
};
use crate::types::{BenchmarkId, ClientId, JobId};
use crate::validated::BatteryLevel;

/// Physical form factor of the device under test.
///
/// Stored as a lowercase string in Parquet (`Utf8`) and validated at the HTTP
/// boundary.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    EnumString,
    AsRefStr,
    EnumIter,
    serde::Deserialize,
    serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DeviceFormFactor {
    Phone,
    Tablet,
    Laptop,
    Desktop,
    Server,
    Embedded,
}

/// Run-environment power state at benchmark time.
///
/// Three states rather than a charging bool: "plugged in but holding" (a
/// charge-limited or full battery) differs from both "topping up" and
/// "on battery", and it matters because plugged-in-not-charging still
/// removes the battery current-limiting that can throttle the SoC.
///
/// `null` (absent on the wire / `None`) means the client didn't report it.
/// Stored as a lowercase string in Parquet (`Utf8`, nullable) and validated
/// at the HTTP boundary by serde enum membership.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    EnumString,
    AsRefStr,
    EnumIter,
    serde::Deserialize,
    serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DevicePowerState {
    /// On external power and the battery is charging.
    Charging,
    /// Running on battery (unplugged), discharging.
    NotCharging,
    /// On external power but not adding charge (battery full or
    /// charge-limited / maintenance).
    PluggedInNotCharging,
}

/// Apple `ProcessInfo.thermalState` (iOS/macOS) — the OS's coarse thermal
/// pressure band, ordered coolest→hottest; a higher state means the OS is (or
/// is about to start) throttling.
///
/// `null` (absent on the wire / `None`) means the client didn't report it (or
/// the device isn't an Apple platform). Stored as a lowercase string in Parquet
/// (`Utf8`, nullable) and validated at the HTTP boundary by serde enum
/// membership.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    EnumString,
    AsRefStr,
    EnumIter,
    serde::Deserialize,
    serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AppleThermalState {
    /// No thermal pressure; the device is operating normally.
    Nominal,
    /// Mild thermal pressure; fans may spin up but performance is unaffected.
    Fair,
    /// The system is actively shedding heat and may be throttling.
    Serious,
    /// Severe thermal pressure; aggressive throttling to prevent damage.
    Critical,
}

/// Android thermal status — the OS `PowerManager.getCurrentThermalStatus()`
/// `THERMAL_STATUS_*` levels, ordered coolest→hottest.
///
/// Mirrors the upstream Android type 1:1. This is a **distinct upstream type**
/// from [`AndroidThrottlingSeverity`] (the thermal-HAL per-sensor severity):
/// the two share the same seven level names but are separate Android APIs, and
/// there is no mapping between them.
///
/// `null` (absent on the wire / `None`) means the client didn't report it (or
/// the device isn't an Android platform). Stored as a lowercase string in
/// Parquet (`Utf8`, nullable) and validated at the HTTP boundary by serde enum
/// membership.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    EnumString,
    AsRefStr,
    EnumIter,
    serde::Deserialize,
    serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AndroidThermalStatus {
    /// No throttling. Serializes to `"none"`.
    None,
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
    Shutdown,
}

/// Android thermal-HAL `ThrottlingSeverity` — the per-sensor throttling
/// severity reported by `android.hardware.thermal`, ordered coolest→hottest.
///
/// Mirrors the upstream Android type 1:1. Same seven levels as
/// [`AndroidThermalStatus`] (the `PowerManager` device-level thermal status),
/// but a **distinct upstream type**: `ThrottlingSeverity` is a per-sensor HAL
/// enum while `AndroidThermalStatus` is the device-level `PowerManager` API.
/// Kept separate to mirror upstream — there is no mapping between the two.
///
/// Stored as a lowercase string in Parquet (`Utf8`) and validated at the HTTP
/// boundary by serde enum membership.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    EnumString,
    AsRefStr,
    EnumIter,
    serde::Deserialize,
    serde::Serialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AndroidThrottlingSeverity {
    /// No throttling. Serializes to `"none"`.
    None,
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
    Shutdown,
}

/// One Android thermal-HAL `Temperature` reading for a single sensor at a
/// single measured repetition. Models the `android.hardware.thermal`
/// `Temperature` parcelable plus the `iteration` it was sampled at. The
/// per-family list flattens every (iteration, sensor) pair — `iteration`
/// tags which repetition each element belongs to. Shared between the wire
/// type ([`crate::submission::SuccessInput`]) and the storage type
/// ([`MetricRow`]); `celsius` is a plain `i32` °C stored as reported.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AndroidTemperatureSensor {
    /// Zero-based index of the measured repetition this reading was sampled at.
    pub iteration: i32,
    /// The sensor's `type` on the wire and in Parquet (renamed because `type`
    /// is a Rust keyword).
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub name: String,
    pub celsius: i32,
    pub throttling_status: AndroidThrottlingSeverity,
}

/// One Linux thermal-zone reading for a single zone at a single measured
/// repetition. Shared between the wire type and the storage type; `iteration`
/// tags the repetition, `celsius` is a plain `i32` °C stored as reported.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LinuxThermalZone {
    /// Zero-based index of the measured repetition this reading was sampled at.
    pub iteration: i32,
    /// The zone's `type` on the wire and in Parquet (renamed because `type` is
    /// a Rust keyword).
    #[serde(rename = "type")]
    pub zone_type: String,
    pub celsius: i32,
}

/// Parquet struct-element fields for an [`AndroidTemperatureSensor`]. Defined once
/// so [`parquet_schema`] and the writer build an identical layout — a
/// `RecordBatch::try_new` type mismatch otherwise.
fn android_sensor_fields() -> Fields {
    Fields::from(vec![
        Field::new("iteration", DataType::Int32, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("celsius", DataType::Int32, false),
        Field::new("throttling_status", DataType::Utf8, false),
    ])
}

/// Parquet struct-element fields for a [`LinuxThermalZone`]. See
/// [`android_sensor_fields`].
fn linux_zone_fields() -> Fields {
    Fields::from(vec![
        Field::new("iteration", DataType::Int32, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("celsius", DataType::Int32, false),
    ])
}

/// The `List` item field wrapping a struct's `fields`. Arrow's default list
/// child name is `item`; matching it keeps the schema and the built array in
/// lock-step. The item is nullable so a row can hold a null element slot.
fn list_item_field(fields: Fields) -> Field {
    Field::new("item", DataType::Struct(fields), true)
}

/// A `List<scalar>` column type carrying one `item` value per measured
/// repetition (the per-iteration series for the scalar thermal families —
/// Apple state, Android status, Android headroom). Item nullable so a row can
/// hold a null slot.
fn list_scalar_type(item: DataType) -> DataType {
    DataType::List(Arc::new(Field::new("item", item, true)))
}

pub fn parquet_schema() -> Schema {
    Schema::new(vec![
        Field::new("result_id", DataType::Utf8, false),
        Field::new("benchmark_id", DataType::Utf8, false),
        Field::new("benchmark_type", DataType::Utf8, false),
        Field::new("metric", DataType::Utf8, false),
        Field::new("client_id", DataType::Utf8, false),
        Field::new("device_name", DataType::Utf8, false),
        Field::new("device_form_factor", DataType::Utf8, false),
        Field::new("device_os_name", DataType::Utf8, false),
        Field::new("device_os_version", DataType::Utf8, false),
        Field::new("device_os_build", DataType::Utf8, true),
        Field::new("device_os_security_patch", DataType::Utf8, true),
        Field::new("device_chip_model", DataType::Utf8, false),
        Field::new("device_gpu_model", DataType::Utf8, true),
        Field::new("device_gpu_vram_bytes", DataType::Int64, true),
        Field::new("device_npu_model", DataType::Utf8, true),
        Field::new("device_npu_vram_bytes", DataType::Int64, true),
        Field::new("device_ram_bytes", DataType::Int64, false),
        // Run-environment power state.
        Field::new("device_battery_level", DataType::Int32, true),
        Field::new("device_power_state", DataType::Utf8, true),
        Field::new("device_power_save_mode", DataType::Boolean, true),
        // Android CPU-scheduling diagnostics (single-valued per submission).
        Field::new("device_android_cpuset", DataType::Utf8, true),
        Field::new("device_android_cpu_affinity_list", DataType::Utf8, true),
        Field::new(
            "device_android_cpu_affinity_excludes_top_tier",
            DataType::Boolean,
            true,
        ),
        // Per-platform per-iteration thermal telemetry, captured around each
        // measured repetition: `_before` at that rep's gate-pass and `_after`
        // once its timed work completes. Every column is a nullable list — a
        // device only populates its own platform's families (Apple state /
        // Android status+headroom+sensors / Linux zones) and leaves the rest
        // `null`. The scalar families are `List<scalar>` with one element per
        // repetition; the sensor/zone families flatten every (iteration,
        // sensor) pair into one `List<Struct>` tagged by `iteration`. The worst
        // condition over a run is derivable from these series downstream, so
        // it is not stored separately.
        Field::new(
            "device_apple_thermal_state_before",
            list_scalar_type(DataType::Utf8),
            true,
        ),
        Field::new(
            "device_apple_thermal_state_after",
            list_scalar_type(DataType::Utf8),
            true,
        ),
        // Raw iOS SoC die temperature (fractional °C), parallel to the Apple
        // thermal-state enum above: same per-repetition cardinality, but a
        // numeric `List<Float32>` rather than the enum's `List<Utf8>`. iOS-only;
        // `null` for every other platform. Stored raw — no rounding or delta.
        Field::new(
            "device_apple_soc_temp_c_before",
            list_scalar_type(DataType::Float32),
            true,
        ),
        Field::new(
            "device_apple_soc_temp_c_after",
            list_scalar_type(DataType::Float32),
            true,
        ),
        Field::new(
            "device_android_thermal_status_before",
            list_scalar_type(DataType::Utf8),
            true,
        ),
        Field::new(
            "device_android_thermal_status_after",
            list_scalar_type(DataType::Utf8),
            true,
        ),
        Field::new(
            "device_android_thermal_headroom_before",
            list_scalar_type(DataType::Float32),
            true,
        ),
        Field::new(
            "device_android_thermal_headroom_after",
            list_scalar_type(DataType::Float32),
            true,
        ),
        Field::new(
            "device_android_thermal_sensors_before",
            DataType::List(Arc::new(list_item_field(android_sensor_fields()))),
            true,
        ),
        Field::new(
            "device_android_thermal_sensors_after",
            DataType::List(Arc::new(list_item_field(android_sensor_fields()))),
            true,
        ),
        Field::new(
            "device_linux_thermal_zones_before",
            DataType::List(Arc::new(list_item_field(linux_zone_fields()))),
            true,
        ),
        Field::new(
            "device_linux_thermal_zones_after",
            DataType::List(Arc::new(list_item_field(linux_zone_fields()))),
            true,
        ),
        Field::new("model_name", DataType::Utf8, true),
        Field::new("model_quant", DataType::Utf8, true),
        Field::new("model_params_total_millions", DataType::Int32, true),
        Field::new("model_params_active_millions", DataType::Int32, true),
        Field::new("model_flags", DataType::Utf8, true),
        Field::new("runtime_name", DataType::Utf8, true),
        Field::new("runtime_version", DataType::Utf8, true),
        Field::new("runtime_flags", DataType::Utf8, true),
        Field::new("runtime_cpu_variant", DataType::Utf8, true),
        Field::new("value", DataType::Float32, false),
        Field::new("unit", DataType::Utf8, false),
        Field::new(
            "submitted_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new(
            "scored_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        // Per-type parameter columns (nullable)
        Field::new("parameter_prefill_tokens", DataType::Int32, true),
        Field::new("parameter_decode_tokens", DataType::Int32, true),
        Field::new("parameter_eval_id", DataType::Utf8, true),
        Field::new("value_stddev", DataType::Float32, true),
        Field::new("parameter_image_width", DataType::Int32, true),
        Field::new("parameter_image_height", DataType::Int32, true),
        Field::new("parameter_text_tokens", DataType::Int32, true),
        Field::new("score_runtime_version", DataType::Utf8, true),
        // Free-form JSON object holding per-run metadata that doesn't
        // belong on the `value` axis — e.g. `{"samples_failed": N}` for
        // eval submissions. Stored as a JSON string for forward
        // compatibility: new keys can be added without a parquet schema
        // change. Nullable so non-eval rows and historical data stay
        // clean.
        Field::new("eval_metadata", DataType::Utf8, true),
        // VL frame count; NULL for non-VL rows. Appended last for back-compat.
        Field::new("parameter_num_images", DataType::Int32, true),
        // Observed VL throughput token counts (measured, not parameters);
        // NULL for non-VL rows. Appended last for back-compat.
        Field::new(
            "observation_vl_throughput_prefill_tokens",
            DataType::Int32,
            true,
        ),
        Field::new(
            "observation_vl_throughput_image_tokens",
            DataType::Int32,
            true,
        ),
        // Full, lossless model / runtime specifications as canonical JSON (keys
        // sorted, whitespace stripped); opaque to the server, with a `_sha256`
        // content id alongside each. NULL when a submission carried only the
        // scalar name/version fields. Appended last for back-compat.
        Field::new("model_descriptor", DataType::Utf8, true),
        Field::new("runtime_descriptor", DataType::Utf8, true),
        Field::new("model_descriptor_sha256", DataType::Utf8, true),
        Field::new("runtime_descriptor_sha256", DataType::Utf8, true),
        // The resolved harness configuration, canonicalized and hashed the same
        // way. Appended last for back-compat.
        Field::new("benchmark_flags", DataType::Utf8, true),
        Field::new("benchmark_flags_sha256", DataType::Utf8, true),
        // Harness version; see `MetricRow::client_version`. Appended last for
        // back-compat.
        Field::new("client_version", DataType::Utf8, true),
        // Content ids for the canonicalized `model_flags` / `runtime_flags`
        // above, so "same model config" / "same runtime config" is a join key
        // rather than a long-string comparison. NULL when the flags are.
        // Appended last for back-compat.
        Field::new("model_flags_sha256", DataType::Utf8, true),
        Field::new("runtime_flags_sha256", DataType::Utf8, true),
        // Peak swap / host memory the run held, reported by every benchmark
        // and denormalized onto every row. NULL from clients that do not
        // sample memory. Appended last for back-compat.
        Field::new("observation_max_swap_bytes", DataType::Int64, true),
        Field::new("observation_max_host_bytes", DataType::Int64, true),
    ])
}

#[derive(Clone)]
pub struct MetricRow {
    pub result_id: String,
    pub benchmark_id: BenchmarkId,
    pub benchmark_type: BenchmarkType,
    pub metric: String,
    pub client_id: ClientId,
    pub device_name: String,
    pub device_form_factor: DeviceFormFactor,
    pub device_os_name: String,
    pub device_os_version: String,
    pub device_os_build: Option<String>,
    pub device_os_security_patch: Option<String>,
    pub device_chip_model: String,
    pub device_gpu_model: Option<String>,
    pub device_gpu_vram_bytes: Option<i64>,
    pub device_npu_model: Option<String>,
    pub device_npu_vram_bytes: Option<i64>,
    pub device_ram_bytes: i64,
    pub device_battery_level: Option<BatteryLevel>,
    pub device_power_state: Option<DevicePowerState>,
    pub device_power_save_mode: Option<bool>,
    // Android CPU-scheduling diagnostics (single-valued per submission). `None` on
    // every non-Android platform and on older clients. `_cpuset` is the cgroup
    // path, `_cpu_affinity_list` a Linux CPU list, `_cpu_affinity_excludes_top_tier`
    // the OEM-demotion signal (highest-freq core tier absent from the allowed set).
    pub device_android_cpuset: Option<String>,
    pub device_android_cpu_affinity_list: Option<String>,
    pub device_android_cpu_affinity_excludes_top_tier: Option<bool>,
    // Per-platform per-iteration thermal telemetry. Each field is `None` unless
    // the device is on the matching platform: Apple (iOS/macOS), Android, or
    // Linux. The scalar families carry one value per measured repetition; the
    // sensor/zone families flatten every (iteration, sensor) pair into one list
    // tagged by `iteration`. `_before` is sampled at each rep's gate-pass,
    // `_after` once its timed work completes. The worst condition over a run is
    // derivable from these series downstream, so it is not stored separately.
    pub device_apple_thermal_state_before: Option<Vec<AppleThermalState>>,
    pub device_apple_thermal_state_after: Option<Vec<AppleThermalState>>,
    /// Raw iOS SoC die temperature (fractional °C), parallel to the Apple
    /// thermal-state enum above. iOS-only; `None` on every other platform.
    /// Stored raw — no rounding, bucketing, or delta.
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
    pub model_name: Option<String>,
    pub model_quant: Option<String>,
    pub model_params_total_millions: Option<i32>,
    pub model_params_active_millions: Option<i32>,
    /// Opaque configuration affecting model behavior. Canonical JSON when the
    /// client sent JSON (keys sorted, whitespace stripped by the ingest path),
    /// otherwise the trimmed string verbatim — the field is documented to
    /// accept either. `None` when absent or a top-level empty object.
    pub model_flags: Option<String>,
    /// Cheap grouping/display runtime name; `None` when the submission omitted
    /// it (the authoritative identity lives in `runtime_descriptor`).
    pub runtime_name: Option<String>,
    /// Cheap grouping/display version; `None` when the submission omitted it
    /// (the authoritative version lives in `runtime_descriptor`).
    pub runtime_version: Option<String>,
    /// Opaque configuration affecting the runtime itself, normalized exactly
    /// like [`Self::model_flags`].
    pub runtime_flags: Option<String>,
    pub runtime_cpu_variant: Option<String>,
    pub value: f32,
    pub value_stddev: Option<f32>,
    pub unit: String,
    pub submitted_at: i64, // microseconds since epoch
    pub scored_at: i64,    // microseconds since epoch
    pub parameter_prefill_tokens: Option<i32>,
    pub parameter_decode_tokens: Option<i32>,
    pub parameter_eval_id: Option<String>,
    pub parameter_image_width: Option<i32>,
    pub parameter_image_height: Option<i32>,
    pub parameter_text_tokens: Option<i32>,
    pub parameter_num_images: Option<i32>,
    /// Observed (measured) VL throughput token counts, not benchmark
    /// parameters: the total prefill length and the image-only portion the
    /// runtime actually produced. These are model-dependent outputs (a 512px
    /// image is ~258 tokens on one encoder, ~1282 on another), so they are
    /// recorded as observations alongside every metric row rather than as
    /// metrics in their own right. NULL for non-VL rows.
    pub observation_vl_throughput_prefill_tokens: Option<i32>,
    pub observation_vl_throughput_image_tokens: Option<i32>,
    pub score_runtime_version: Option<String>,
    /// JSON-encoded `{key: value}` object holding per-run metadata that
    /// isn't a metric in its own right. Currently carries
    /// `{"samples_failed": N}` for eval submissions where any sample
    /// crashed client-side (pipette-clients#103). `None` for rows that
    /// have no metadata to record.
    pub eval_metadata: Option<String>,
    /// Full, lossless model specification as canonical JSON (keys sorted,
    /// whitespace stripped by the ingest path). Opaque to the server — never
    /// deserialized, only stored and pattern-searched. `None` when the
    /// submission carried only `model_name`.
    pub model_descriptor: Option<String>,
    /// Full, lossless runtime specification as canonical JSON — the runtime
    /// counterpart to [`Self::model_descriptor`], with version/build baked in. `None`
    /// when the submission carried only `runtime_name` / `runtime_version`.
    pub runtime_descriptor: Option<String>,
    /// Hex `sha256` of the canonical `model_descriptor` (derived server-side,
    /// not client-supplied); `None` when absent.
    pub model_descriptor_sha256: Option<String>,
    /// Hex `sha256` of the canonical `runtime_descriptor` (derived server-side,
    /// not client-supplied); `None` when absent.
    pub runtime_descriptor_sha256: Option<String>,
    /// The resolved harness configuration the run executed under, as canonical
    /// JSON — readiness gating, timeouts, loop detection. `None` from a client
    /// that does not report it.
    pub benchmark_flags: Option<String>,
    /// Hex `sha256` of the canonical `benchmark_flags` (derived server-side,
    /// not client-supplied); the grouping key for "runs measured the same
    /// way". `None` when absent.
    pub benchmark_flags_sha256: Option<String>,
    /// Version of the submitting client build (the benchmark harness), as
    /// reported on the wire. Distinct from [`Self::runtime_version`], which is
    /// the inference runtime the client drove: the same runtime measured by two
    /// client versions can differ if the harness changed how it measures.
    /// `None` from clients that don't report it.
    pub client_version: Option<String>,
    /// Hex `sha256` of the canonical `model_flags` (derived server-side, not
    /// client-supplied); `None` when absent.
    pub model_flags_sha256: Option<String>,
    /// Hex `sha256` of the canonical `runtime_flags` (derived server-side, not
    /// client-supplied); the grouping key for "runs configured the same way".
    /// `None` when absent.
    pub runtime_flags_sha256: Option<String>,
    /// Peak swap and host memory that the run held, in bytes. Every benchmark
    /// type reports them, so the scorer denormalizes them onto every row of the
    /// submission. `None` from clients that do not sample memory.
    ///
    /// The swap term is contained in the host peak rather than additional to
    /// it, so the two are never summed. `Some(0)` swap is a real reading — the
    /// platform sampled swap and the run stayed resident — while `None` means
    /// nothing sampled it.
    ///
    /// [`Self::observation_max_host_bytes`] is not the `max_host_usage` metric.
    /// The wire field `max_host_bytes` is the measurement that the peak-memory
    /// benchmarks require, and it lands on the `value` axis of its own row.
    /// This field is a per-run observation that every row carries, and it
    /// counts compressed and paged-out memory where the platform exposes it.
    /// The two therefore disagree by design on a peak-memory row.
    pub observation_max_swap_bytes: Option<i64>,
    pub observation_max_host_bytes: Option<i64>,
}

#[cfg(test)]
impl Default for MetricRow {
    /// Test-only skeleton: valid required identity/metric fields with everything
    /// optional/contextual defaulted (None / 0 / empty). Tests override only the
    /// fields they care about via `..Default::default()`. Not compiled in
    /// production (the validated id newtypes have no meaningful empty default).
    fn default() -> Self {
        Self {
            result_id: "r1".into(),
            benchmark_id: BenchmarkId::try_new("test").expect("valid"),
            benchmark_type: BenchmarkType::PrefillThroughput,
            metric: String::new(),
            client_id: ClientId::try_new("ev1_c").expect("valid"),
            device_name: "test-device".into(),
            device_form_factor: DeviceFormFactor::Embedded,
            device_os_name: "Linux".into(),
            device_os_version: "22.04".into(),
            device_os_build: None,
            device_os_security_patch: None,
            device_chip_model: "test-chip".into(),
            device_gpu_model: None,
            device_gpu_vram_bytes: None,
            device_npu_model: None,
            device_npu_vram_bytes: None,
            device_ram_bytes: 17_179_869_184,
            device_battery_level: None,
            device_power_state: None,
            device_power_save_mode: None,
            device_android_cpuset: None,
            device_android_cpu_affinity_list: None,
            device_android_cpu_affinity_excludes_top_tier: None,
            device_apple_thermal_state_before: None,
            device_apple_thermal_state_after: None,
            device_apple_soc_temp_c_before: None,
            device_apple_soc_temp_c_after: None,
            device_android_thermal_status_before: None,
            device_android_thermal_status_after: None,
            device_android_thermal_headroom_before: None,
            device_android_thermal_headroom_after: None,
            device_android_thermal_sensors_before: None,
            device_android_thermal_sensors_after: None,
            device_linux_thermal_zones_before: None,
            device_linux_thermal_zones_after: None,
            model_name: Some("model".into()),
            model_quant: Some("q4".into()),
            model_params_total_millions: None,
            model_params_active_millions: None,
            model_flags: None,
            runtime_name: Some("rt".into()),
            runtime_version: Some("v1".into()),
            runtime_flags: None,
            runtime_cpu_variant: None,
            value: 0.0,
            value_stddev: None,
            unit: String::new(),
            submitted_at: 0,
            scored_at: 0,
            parameter_prefill_tokens: None,
            parameter_decode_tokens: None,
            parameter_eval_id: None,
            parameter_image_width: None,
            parameter_image_height: None,
            parameter_text_tokens: None,
            parameter_num_images: None,
            observation_vl_throughput_prefill_tokens: None,
            observation_vl_throughput_image_tokens: None,
            score_runtime_version: None,
            eval_metadata: None,
            model_descriptor: None,
            runtime_descriptor: None,
            model_descriptor_sha256: None,
            runtime_descriptor_sha256: None,
            benchmark_flags: None,
            benchmark_flags_sha256: None,
            model_flags_sha256: None,
            runtime_flags_sha256: None,
            client_version: None,
            observation_max_swap_bytes: None,
            observation_max_host_bytes: None,
        }
    }
}

/// Derive a `YYYY-MM-DD` day key from a microsecond-precision UTC timestamp.
pub(crate) fn day_key_from_timestamp(timestamp_us: i64) -> anyhow::Result<String> {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = DateTime::from_timestamp_micros(timestamp_us)
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp: {timestamp_us}"))?;
    Ok(dt.format("%Y-%m-%d").to_string())
}

/// New writes use per-**day** partitions (`day={YYYY-MM-DD}`). Legacy
/// `month={YYYY-MM}` partitions are never written again — reads union
/// both (see [`read_metrics_for_job`]). Per-day caps each partition
/// read-modify-write to one day's rows instead of a whole month's.
pub fn warehouse_day_partition_dir(
    warehouse_dir: &Path,
    benchmark_id: &BenchmarkId,
    client_id: &ClientId,
    day_key: &str,
) -> std::path::PathBuf {
    warehouse_dir
        .join(format!("benchmark_id={benchmark_id}"))
        .join(format!("client_id={client_id}"))
        .join(format!("day={day_key}"))
}

/// Legacy `month={YYYY-MM}` partition dir. **Not used by the production
/// write path** (which writes `day=` — see [`warehouse_day_partition_dir`]);
/// retained for reading and generating legacy fixtures.
pub fn warehouse_month_partition_dir(
    warehouse_dir: &Path,
    benchmark_id: &BenchmarkId,
    client_id: &ClientId,
    month_key: &str,
) -> std::path::PathBuf {
    warehouse_dir
        .join(format!("benchmark_id={benchmark_id}"))
        .join(format!("client_id={client_id}"))
        .join(format!("month={month_key}"))
}

/// Parse a partition directory's Hive key into the date used to order and
/// window it: a `day=` key is its own date; a legacy `month=` key maps to
/// the **last day** of that month (so the month overlaps the window
/// whenever any of its days would). Unknown keys yield `None` and are
/// skipped.
fn partition_sort_date(key: &str) -> Option<chrono::NaiveDate> {
    use chrono::{Datelike, NaiveDate};
    if let Some(d) = key.strip_prefix("day=") {
        return NaiveDate::parse_from_str(d, "%Y-%m-%d").ok();
    }
    if let Some(m) = key.strip_prefix("month=") {
        let first = NaiveDate::parse_from_str(&format!("{m}-01"), "%Y-%m-%d").ok()?;
        // Last day of the month = day before the first of the next month.
        let next_month = if first.month() == 12 {
            NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
        }?;
        return next_month.pred_opt();
    }
    None
}

/// Select the partitions a job-metrics read should scan, in scan order.
///
/// Keeps only partitions whose date overlaps `[today - read_days, today]`
/// (so the count is bounded by the window), then orders **all `day=`
/// partitions first (newest→oldest), then legacy `month=` partitions
/// (newest→oldest)**. New data only ever lands in `day=`, so checking days
/// first finds recent jobs fast and consults the frozen month partitions
/// only as a fallback. It also breaks the same-date tie deterministically
/// in favor of `day=`: a job written to both schemes (scored just before
/// the per-day cutover, re-scored just after) resolves to its newer `day=`
/// copy. A wide `read_days` (e.g. in tests) effectively scans everything.
pub fn select_partitions_to_scan(
    keys: impl IntoIterator<Item = String>,
    read_days: u32,
    today: chrono::NaiveDate,
) -> Vec<String> {
    let cutoff = today - chrono::Duration::days(i64::from(read_days));
    let mut selected: Vec<(bool, chrono::NaiveDate, String)> = keys
        .into_iter()
        .filter_map(|k| partition_sort_date(&k).map(|d| (k.starts_with("month="), d, k)))
        .filter(|(_, d, _)| *d >= cutoff)
        .collect();
    // `is_month` ascending puts day partitions first; within a scheme,
    // newest date first.
    selected.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    selected.into_iter().map(|(_, _, k)| k).collect()
}

pub fn append_to_parquet(opts: WriterOpts, path: &Path, rows: &[MetricRow]) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let schema = Arc::new(parquet_schema());

    // Read existing batches if file exists, normalizing old-schema batches
    // by adding missing nullable columns as all-null arrays.
    let mut all_batches: Vec<RecordBatch> = if path.exists() {
        read_batches_from_file(path)?
            .map(|batch| normalize_batch(&schema, &batch?))
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    all_batches.push(rows_to_batch(&schema, rows)?);
    write_batches_to_file(opts, path, schema, &all_batches)
}

/// Pad a RecordBatch to match the target schema by adding all-null columns
/// for any nullable fields present in the target but missing from the batch.
/// Errors if the batch is missing a non-nullable column or contains columns
/// not present in the target schema (to avoid silent data loss).
fn normalize_batch(
    target_schema: &Arc<Schema>,
    batch: &RecordBatch,
) -> anyhow::Result<RecordBatch> {
    if batch.schema().fields() == target_schema.fields() {
        return Ok(batch.clone());
    }

    // Reject extra columns that would be silently dropped.
    for field in batch.schema().fields() {
        if target_schema.field_with_name(field.name()).is_err() {
            anyhow::bail!(
                "parquet file contains unknown column '{}' not in current schema",
                field.name()
            );
        }
    }

    let num_rows = batch.num_rows();
    let mut columns = Vec::with_capacity(target_schema.fields().len());
    for field in target_schema.fields() {
        match batch.column_by_name(field.name()) {
            Some(col) => columns.push(col.clone()),
            None if field.is_nullable() => {
                columns.push(new_null_array(field.data_type(), num_rows));
            }
            None => {
                anyhow::bail!(
                    "old parquet file is missing required column '{}'",
                    field.name()
                );
            }
        }
    }

    Ok(RecordBatch::try_new(target_schema.clone(), columns)?)
}

pub fn write_partition_part(
    opts: WriterOpts,
    partition_dir: &Path,
    part_name: &str,
    rows: &[MetricRow],
) -> anyhow::Result<()> {
    append_to_parquet(opts, &partition_dir.join(part_name), rows)
}

/// Append `new_rows` to a partition as size-capped part files.
///
/// **Append-only, no dedup.** The tail `part-NNNN.parquet` is topped up to
/// `max_rows_per_part`, then overflow rolls into fresh parts; earlier parts are
/// never read or rewritten, so the write cost is `O(max_rows_per_part + new)`
/// rather than `O(partition)`. Re-scoring the same `job_id` (the rare
/// crash-before-`mark_processed` retry) therefore *appends* a second copy
/// rather than replacing the first — readers must tolerate that. See
/// `docs/storage.md`.
pub(crate) fn write_partition(
    opts: WriterOpts,
    partition_dir: &Path,
    new_rows: &[MetricRow],
    max_rows_per_part: usize,
) -> anyhow::Result<()> {
    if new_rows.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(partition_dir)?;

    let (next_index, mut carry) = match tail_part(partition_dir)? {
        Some((idx, rows)) if rows.len() < max_rows_per_part => (idx, rows),
        Some((idx, _)) => (idx + 1, Vec::new()),
        None => (1, Vec::new()),
    };

    carry.extend(new_rows.iter().cloned());
    carry
        .chunks(max_rows_per_part)
        .enumerate()
        .try_for_each(|(i, chunk)| {
            let path = partition_dir.join(format!("part-{:04}.parquet", next_index + i));
            write_parquet_atomic(opts, &path, chunk)
        })
}

/// The highest-indexed `part-NNNN.parquet` in a partition and its rows, or
/// `None` when the partition has no part files yet.
fn tail_part(partition_dir: &Path) -> anyhow::Result<Option<(usize, Vec<MetricRow>)>> {
    let best = std::fs::read_dir(partition_dir)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|entry| {
            let path = entry.path();
            let idx = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(part_index)?;
            Some((idx, path))
        })
        .max_by_key(|(idx, _)| *idx);

    best.map(|(idx, path)| Ok((idx, read_part_rows(&path)?)))
        .transpose()
}

/// Read every metric row from a single part file.
fn read_part_rows(path: &Path) -> anyhow::Result<Vec<MetricRow>> {
    Ok(read_batches_from_file(path)?
        .map(|b| batch_to_rows(&b?))
        .collect::<anyhow::Result<Vec<_>>>()?
        .concat())
}

/// Parse the `NNNN` index out of a `part-NNNN.parquet` filename.
pub(crate) fn part_index(file_name: &str) -> Option<usize> {
    file_name
        .strip_prefix("part-")
        .and_then(|s| s.strip_suffix(".parquet"))
        .and_then(|s| s.parse::<usize>().ok())
}

/// Write one part file via tmp+rename so replacing the tail part is atomic.
/// The temp file uses a `.tmp` (not `.parquet`) extension so a crash before
/// the rename can't leave an orphan that the `.parquet`-only readers would
/// pick up as a real part file.
fn write_parquet_atomic(opts: WriterOpts, path: &Path, rows: &[MetricRow]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("part path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()));
    write_parquet(opts, &tmp, rows)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write_parquet(opts: WriterOpts, path: &Path, rows: &[MetricRow]) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let schema = Arc::new(parquet_schema());
    let batch = rows_to_batch(&schema, rows)?;
    write_batches_to_file(opts, path, schema, &[batch])
}

/// Serialize metric rows to Parquet bytes in memory.
pub(crate) fn rows_to_parquet_bytes(
    opts: WriterOpts,
    rows: &[MetricRow],
) -> anyhow::Result<Vec<u8>> {
    let schema = Arc::new(parquet_schema());
    let batch = rows_to_batch(&schema, rows)?;
    write_batch_bytes(opts, schema, &batch)
}

/// Deserialize metric rows from Parquet bytes.
pub(crate) fn rows_from_parquet_bytes(data: &[u8]) -> anyhow::Result<Vec<MetricRow>> {
    let mut rows = Vec::new();
    for batch in read_batches_from_bytes(data)? {
        rows.extend(batch_to_rows(&batch?)?);
    }
    Ok(rows)
}

/// Read every row across all part files of a partition. Test-only — the
/// append write path reads just the tail part, not the whole partition.
#[cfg(test)]
fn read_partition_rows(partition_dir: &Path) -> anyhow::Result<Vec<MetricRow>> {
    if !partition_dir.exists() {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    for entry in std::fs::read_dir(partition_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "parquet") {
            continue;
        }
        for batch in read_batches_from_file(&path)? {
            rows.extend(batch_to_rows(&batch?)?);
        }
    }

    Ok(rows)
}

/// Downcast a struct child to `StringArray`, with a clear error naming both
/// the list column and the offending child field.
fn struct_string_child<'a>(
    structs: &'a StructArray,
    child: &str,
    col: &str,
) -> anyhow::Result<&'a StringArray> {
    structs
        .column_by_name(child)
        .ok_or_else(|| anyhow::anyhow!("{col} column struct missing '{child}' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("{col} column '{child}' field has unexpected type"))
}

/// Downcast a struct child to `Int32Array` (see [`struct_string_child`]).
fn struct_i32_child<'a>(
    structs: &'a StructArray,
    child: &str,
    col: &str,
) -> anyhow::Result<&'a Int32Array> {
    structs
        .column_by_name(child)
        .ok_or_else(|| anyhow::anyhow!("{col} column struct missing '{child}' field"))?
        .as_any()
        .downcast_ref::<Int32Array>()
        .ok_or_else(|| anyhow::anyhow!("{col} column '{child}' field has unexpected type"))
}

/// Read a `List<Struct>` sensor column into one `Option<Vec<_>>` per row. A
/// column absent from an old on-disk schema yields all-`None` (lenient, like
/// the scalar reads); a null list slot yields `None` for that row.
fn read_android_sensors_column(
    batch: &RecordBatch,
    name: &str,
) -> anyhow::Result<Vec<Option<Vec<AndroidTemperatureSensor>>>> {
    let num_rows = batch.num_rows();
    let Some(col) = batch.column_by_name(name) else {
        return Ok(vec![None; num_rows]);
    };
    let list = col
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))?;
    (0..num_rows)
        .map(|i| {
            if list.is_null(i) {
                return Ok(None);
            }
            let values = list.value(i);
            let structs = values
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| anyhow::anyhow!("{name} column items have unexpected type"))?;
            let iterations = struct_i32_child(structs, "iteration", name)?;
            let types = struct_string_child(structs, "type", name)?;
            let names = struct_string_child(structs, "name", name)?;
            let celsius = struct_i32_child(structs, "celsius", name)?;
            let statuses = struct_string_child(structs, "throttling_status", name)?;
            let elems = (0..structs.len())
                .map(|j| {
                    Ok(AndroidTemperatureSensor {
                        iteration: iterations.value(j),
                        sensor_type: types.value(j).to_string(),
                        name: names.value(j).to_string(),
                        celsius: celsius.value(j),
                        throttling_status: statuses
                            .value(j)
                            .parse::<AndroidThrottlingSeverity>()
                            .map_err(|e| anyhow::anyhow!("{name}: {e}"))?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Some(elems))
        })
        .collect()
}

/// Read a `List<Struct>` Linux-zone column into one `Option<Vec<_>>` per row.
/// See [`read_android_sensors_column`].
fn read_linux_zones_column(
    batch: &RecordBatch,
    name: &str,
) -> anyhow::Result<Vec<Option<Vec<LinuxThermalZone>>>> {
    let num_rows = batch.num_rows();
    let Some(col) = batch.column_by_name(name) else {
        return Ok(vec![None; num_rows]);
    };
    let list = col
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))?;
    (0..num_rows)
        .map(|i| {
            if list.is_null(i) {
                return Ok(None);
            }
            let values = list.value(i);
            let structs = values
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| anyhow::anyhow!("{name} column items have unexpected type"))?;
            let iterations = struct_i32_child(structs, "iteration", name)?;
            let types = struct_string_child(structs, "type", name)?;
            let celsius = struct_i32_child(structs, "celsius", name)?;
            let elems = (0..structs.len())
                .map(|j| LinuxThermalZone {
                    iteration: iterations.value(j),
                    zone_type: types.value(j).to_string(),
                    celsius: celsius.value(j),
                })
                .collect::<Vec<_>>();
            Ok(Some(elems))
        })
        .collect()
}

/// Read a `List<Utf8>` per-iteration column into one `Option<Vec<T>>` per row,
/// parsing each element string into the target enum `T`. Absent column → all
/// `None`; null list slot → row `None`.
fn read_enum_list_column<T>(batch: &RecordBatch, name: &str) -> anyhow::Result<Vec<Option<Vec<T>>>>
where
    T: std::str::FromStr + Clone,
    T::Err: std::fmt::Display,
{
    let num_rows = batch.num_rows();
    let Some(col) = batch.column_by_name(name) else {
        return Ok(vec![None; num_rows]);
    };
    let list = col
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))?;
    (0..num_rows)
        .map(|i| {
            if list.is_null(i) {
                return Ok(None);
            }
            let values = list.value(i);
            let strs = values
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("{name} column items have unexpected type"))?;
            let elems = (0..strs.len())
                .map(|j| {
                    strs.value(j)
                        .parse::<T>()
                        .map_err(|e| anyhow::anyhow!("{name}: {e}"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Some(elems))
        })
        .collect()
}

/// Read a `List<Float32>` per-iteration column into one `Option<Vec<f32>>` per
/// row. See [`read_enum_list_column`].
fn read_f32_list_column(batch: &RecordBatch, name: &str) -> anyhow::Result<Vec<Option<Vec<f32>>>> {
    let num_rows = batch.num_rows();
    let Some(col) = batch.column_by_name(name) else {
        return Ok(vec![None; num_rows]);
    };
    let list = col
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))?;
    (0..num_rows)
        .map(|i| {
            if list.is_null(i) {
                return Ok(None);
            }
            let values = list.value(i);
            let floats = values
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| anyhow::anyhow!("{name} column items have unexpected type"))?;
            Ok(Some((0..floats.len()).map(|j| floats.value(j)).collect()))
        })
        .collect()
}

/// Build a `List<scalar>` array from a per-row optional series. Offsets bound
/// each row's slice and a null buffer marks rows whose `Option` is `None`; the
/// item field is nullable but present rows carry all-present values.
fn build_scalar_list_array(
    item: DataType,
    values: ArrayRef,
    lists_len: &[Option<usize>],
) -> ArrayRef {
    let validity: Vec<bool> = lists_len.iter().map(|l| l.is_some()).collect();
    let offsets: Vec<i32> = std::iter::once(0)
        .chain(lists_len.iter().scan(0i32, |end, len| {
            *end += len.unwrap_or(0) as i32;
            Some(*end)
        }))
        .collect();
    Arc::new(ListArray::new(
        Arc::new(Field::new("item", item, true)),
        OffsetBuffer::new(offsets.into()),
        values,
        Some(NullBuffer::from(validity)),
    ))
}

/// Build a `List<Utf8>` array for a scalar-enum per-iteration column.
fn build_string_list_array(lists: &[Option<Vec<&str>>]) -> ArrayRef {
    let lens: Vec<Option<usize>> = lists.iter().map(|l| l.as_ref().map(Vec::len)).collect();
    let values: Vec<&str> = lists.iter().flatten().flatten().copied().collect();
    build_scalar_list_array(DataType::Utf8, Arc::new(StringArray::from(values)), &lens)
}

/// Build a `List<Float32>` array for the headroom per-iteration columns.
fn build_f32_list_array(lists: &[Option<Vec<f32>>]) -> ArrayRef {
    let lens: Vec<Option<usize>> = lists.iter().map(|l| l.as_ref().map(Vec::len)).collect();
    let values: Vec<f32> = lists.iter().flatten().flatten().copied().collect();
    build_scalar_list_array(
        DataType::Float32,
        Arc::new(Float32Array::from(values)),
        &lens,
    )
}

/// Build a `List<Struct>` array for the Android sensor columns. Elements are
/// flattened into the struct child arrays; per-row offsets bound each slot and
/// a null buffer marks rows whose `Option` is `None`.
fn build_android_sensors_array(lists: &[Option<Vec<AndroidTemperatureSensor>>]) -> ArrayRef {
    let validity: Vec<bool> = lists.iter().map(|l| l.is_some()).collect();
    let offsets: Vec<i32> = std::iter::once(0)
        .chain(lists.iter().scan(0i32, |end, list| {
            *end += list.as_ref().map_or(0, |e| e.len()) as i32;
            Some(*end)
        }))
        .collect();
    let sensors = || lists.iter().flatten().flatten();
    let iterations: Vec<i32> = sensors().map(|e| e.iteration).collect();
    let types: Vec<&str> = sensors().map(|e| e.sensor_type.as_str()).collect();
    let names: Vec<&str> = sensors().map(|e| e.name.as_str()).collect();
    let celsius: Vec<i32> = sensors().map(|e| e.celsius).collect();
    let statuses: Vec<&str> = sensors().map(|e| e.throttling_status.as_ref()).collect();
    let structs = StructArray::new(
        android_sensor_fields(),
        vec![
            Arc::new(Int32Array::from(iterations)) as ArrayRef,
            Arc::new(StringArray::from(types)),
            Arc::new(StringArray::from(names)),
            Arc::new(Int32Array::from(celsius)),
            Arc::new(StringArray::from(statuses)),
        ],
        None,
    );
    Arc::new(ListArray::new(
        Arc::new(list_item_field(android_sensor_fields())),
        OffsetBuffer::new(offsets.into()),
        Arc::new(structs),
        Some(NullBuffer::from(validity)),
    ))
}

/// Build a `List<Struct>` array for the Linux-zone columns. See
/// [`build_android_sensors_array`].
fn build_linux_zones_array(lists: &[Option<Vec<LinuxThermalZone>>]) -> ArrayRef {
    let validity: Vec<bool> = lists.iter().map(|l| l.is_some()).collect();
    let offsets: Vec<i32> = std::iter::once(0)
        .chain(lists.iter().scan(0i32, |end, list| {
            *end += list.as_ref().map_or(0, |e| e.len()) as i32;
            Some(*end)
        }))
        .collect();
    let zones = || lists.iter().flatten().flatten();
    let iterations: Vec<i32> = zones().map(|e| e.iteration).collect();
    let types: Vec<&str> = zones().map(|e| e.zone_type.as_str()).collect();
    let celsius: Vec<i32> = zones().map(|e| e.celsius).collect();
    let structs = StructArray::new(
        linux_zone_fields(),
        vec![
            Arc::new(Int32Array::from(iterations)) as ArrayRef,
            Arc::new(StringArray::from(types)),
            Arc::new(Int32Array::from(celsius)),
        ],
        None,
    );
    Arc::new(ListArray::new(
        Arc::new(list_item_field(linux_zone_fields())),
        OffsetBuffer::new(offsets.into()),
        Arc::new(structs),
        Some(NullBuffer::from(validity)),
    ))
}

fn batch_to_rows(batch: &RecordBatch) -> anyhow::Result<Vec<MetricRow>> {
    let result_ids = batch
        .column_by_name("result_id")
        .ok_or_else(|| anyhow::anyhow!("missing result_id column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("result_id column has unexpected type"))?;
    let benchmark_ids = batch
        .column_by_name("benchmark_id")
        .ok_or_else(|| anyhow::anyhow!("missing benchmark_id column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("benchmark_id column has unexpected type"))?;
    let benchmark_types = batch
        .column_by_name("benchmark_type")
        .ok_or_else(|| anyhow::anyhow!("missing benchmark_type column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("benchmark_type column has unexpected type"))?;
    let metrics = batch
        .column_by_name("metric")
        .ok_or_else(|| anyhow::anyhow!("missing metric column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("metric column has unexpected type"))?;
    let client_ids = batch
        .column_by_name("client_id")
        .ok_or_else(|| anyhow::anyhow!("missing client_id column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("client_id column has unexpected type"))?;
    let device_names = batch
        .column_by_name("device_name")
        .ok_or_else(|| anyhow::anyhow!("missing device_name column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_name column has unexpected type"))?;
    let device_form_factors = batch
        .column_by_name("device_form_factor")
        .ok_or_else(|| anyhow::anyhow!("missing device_form_factor column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_form_factor column has unexpected type"))?;
    let device_os_names = batch
        .column_by_name("device_os_name")
        .ok_or_else(|| anyhow::anyhow!("missing device_os_name column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_os_name column has unexpected type"))?;
    let device_os_versions = batch
        .column_by_name("device_os_version")
        .ok_or_else(|| anyhow::anyhow!("missing device_os_version column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_os_version column has unexpected type"))?;
    let device_chip_models = batch
        .column_by_name("device_chip_model")
        .ok_or_else(|| anyhow::anyhow!("missing device_chip_model column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_chip_model column has unexpected type"))?;
    // Lenient: parquet written before these columns existed reads back as
    // all-null (same pattern as the battery/power columns below).
    let device_os_build_col = batch
        .column_by_name("device_os_build")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("device_os_build column has unexpected type"))
        })
        .transpose()?;
    let device_os_security_patch_col = batch
        .column_by_name("device_os_security_patch")
        .map(|c| {
            c.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
                anyhow::anyhow!("device_os_security_patch column has unexpected type")
            })
        })
        .transpose()?;
    let device_gpu_models = batch
        .column_by_name("device_gpu_model")
        .ok_or_else(|| anyhow::anyhow!("missing device_gpu_model column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_gpu_model column has unexpected type"))?;
    let device_gpu_vram_bytes = batch
        .column_by_name("device_gpu_vram_bytes")
        .ok_or_else(|| anyhow::anyhow!("missing device_gpu_vram_bytes column"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow::anyhow!("device_gpu_vram_bytes column has unexpected type"))?;
    let device_npu_models = batch
        .column_by_name("device_npu_model")
        .ok_or_else(|| anyhow::anyhow!("missing device_npu_model column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("device_npu_model column has unexpected type"))?;
    let device_npu_vram_bytes = batch
        .column_by_name("device_npu_vram_bytes")
        .ok_or_else(|| anyhow::anyhow!("missing device_npu_vram_bytes column"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow::anyhow!("device_npu_vram_bytes column has unexpected type"))?;
    let device_ram_bytes = batch
        .column_by_name("device_ram_bytes")
        .ok_or_else(|| anyhow::anyhow!("missing device_ram_bytes column"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow::anyhow!("device_ram_bytes column has unexpected type"))?;
    // Lenient: parquet written before these columns existed reads back as
    // all-null (same pattern as the optional parameter columns below).
    let device_battery_level_col = batch
        .column_by_name("device_battery_level")
        .map(|c| {
            c.as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| anyhow::anyhow!("device_battery_level column has unexpected type"))
        })
        .transpose()?;
    let device_power_state_col = batch
        .column_by_name("device_power_state")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("device_power_state column has unexpected type"))
        })
        .transpose()?;
    let device_power_save_mode_col = batch
        .column_by_name("device_power_save_mode")
        .map(|c| {
            c.as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(|| anyhow::anyhow!("device_power_save_mode column has unexpected type"))
        })
        .transpose()?;
    let device_android_cpuset_col = batch
        .column_by_name("device_android_cpuset")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("device_android_cpuset column has unexpected type"))
        })
        .transpose()?;
    let device_android_cpu_affinity_list_col = batch
        .column_by_name("device_android_cpu_affinity_list")
        .map(|c| {
            c.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
                anyhow::anyhow!("device_android_cpu_affinity_list column has unexpected type")
            })
        })
        .transpose()?;
    let device_android_cpu_affinity_excludes_top_tier_col = batch
        .column_by_name("device_android_cpu_affinity_excludes_top_tier")
        .map(|c| {
            c.as_any().downcast_ref::<BooleanArray>().ok_or_else(|| {
                anyhow::anyhow!(
                    "device_android_cpu_affinity_excludes_top_tier column has unexpected type"
                )
            })
        })
        .transpose()?;
    // Lenient: parquet written before the thermal columns existed reads back
    // as all-null (same pattern as the power-state columns above). Each device
    // only populates its own platform's columns; the rest stay null.
    // Per-iteration thermal telemetry: each family is one `Option<Vec<_>>` per
    // row (null list / absent-on-old-schema column → row `None`). Scalar
    // families read + parse via the list helpers; sensor/zone families via the
    // `List<Struct>` helpers.
    let device_apple_thermal_state_before_col =
        read_enum_list_column::<AppleThermalState>(batch, "device_apple_thermal_state_before")?;
    let device_apple_thermal_state_after_col =
        read_enum_list_column::<AppleThermalState>(batch, "device_apple_thermal_state_after")?;
    let device_apple_soc_temp_c_before_col =
        read_f32_list_column(batch, "device_apple_soc_temp_c_before")?;
    let device_apple_soc_temp_c_after_col =
        read_f32_list_column(batch, "device_apple_soc_temp_c_after")?;
    let device_android_thermal_status_before_col = read_enum_list_column::<AndroidThermalStatus>(
        batch,
        "device_android_thermal_status_before",
    )?;
    let device_android_thermal_status_after_col = read_enum_list_column::<AndroidThermalStatus>(
        batch,
        "device_android_thermal_status_after",
    )?;
    let device_android_thermal_headroom_before_col =
        read_f32_list_column(batch, "device_android_thermal_headroom_before")?;
    let device_android_thermal_headroom_after_col =
        read_f32_list_column(batch, "device_android_thermal_headroom_after")?;
    let device_android_thermal_sensors_before_col =
        read_android_sensors_column(batch, "device_android_thermal_sensors_before")?;
    let device_android_thermal_sensors_after_col =
        read_android_sensors_column(batch, "device_android_thermal_sensors_after")?;
    let device_linux_thermal_zones_before_col =
        read_linux_zones_column(batch, "device_linux_thermal_zones_before")?;
    let device_linux_thermal_zones_after_col =
        read_linux_zones_column(batch, "device_linux_thermal_zones_after")?;
    let model_names = batch
        .column_by_name("model_name")
        .ok_or_else(|| anyhow::anyhow!("missing model_name column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("model_name column has unexpected type"))?;
    let model_quants = batch
        .column_by_name("model_quant")
        .ok_or_else(|| anyhow::anyhow!("missing model_quant column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("model_quant column has unexpected type"))?;
    let model_params_total_millions_col = batch
        .column_by_name("model_params_total_millions")
        .map(|c| {
            c.as_any().downcast_ref::<Int32Array>().ok_or_else(|| {
                anyhow::anyhow!("model_params_total_millions column has unexpected type")
            })
        })
        .transpose()?;
    let model_params_active_millions_col = batch
        .column_by_name("model_params_active_millions")
        .map(|c| {
            c.as_any().downcast_ref::<Int32Array>().ok_or_else(|| {
                anyhow::anyhow!("model_params_active_millions column has unexpected type")
            })
        })
        .transpose()?;
    // Appended column; may be absent in older parquet files, so read it
    // optionally like the other back-compat-appended columns.
    let model_descriptors = batch
        .column_by_name("model_descriptor")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("model_descriptor column has unexpected type"))
        })
        .transpose()?;
    let runtime_descriptors = batch
        .column_by_name("runtime_descriptor")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("runtime_descriptor column has unexpected type"))
        })
        .transpose()?;
    let model_descriptor_sha256s = batch
        .column_by_name("model_descriptor_sha256")
        .map(|c| {
            c.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
                anyhow::anyhow!("model_descriptor_sha256 column has unexpected type")
            })
        })
        .transpose()?;
    let runtime_descriptor_sha256s = batch
        .column_by_name("runtime_descriptor_sha256")
        .map(|c| {
            c.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
                anyhow::anyhow!("runtime_descriptor_sha256 column has unexpected type")
            })
        })
        .transpose()?;
    let benchmark_flags_col = batch
        .column_by_name("benchmark_flags")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("benchmark_flags column has unexpected type"))
        })
        .transpose()?;
    let benchmark_flags_sha256s = batch
        .column_by_name("benchmark_flags_sha256")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("benchmark_flags_sha256 column has unexpected type"))
        })
        .transpose()?;
    let client_versions = batch
        .column_by_name("client_version")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("client_version column has unexpected type"))
        })
        .transpose()?;
    // Optional: absent in parquet written before the flag hashes existed. Read
    // as None so `fix-canonical` can backfill them.
    let model_flags_sha256s = batch
        .column_by_name("model_flags_sha256")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("model_flags_sha256 column has unexpected type"))
        })
        .transpose()?;
    let runtime_flags_sha256s = batch
        .column_by_name("runtime_flags_sha256")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("runtime_flags_sha256 column has unexpected type"))
        })
        .transpose()?;
    let model_flags = batch
        .column_by_name("model_flags")
        .ok_or_else(|| anyhow::anyhow!("missing model_flags column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("model_flags column has unexpected type"))?;
    let runtime_names = batch
        .column_by_name("runtime_name")
        .ok_or_else(|| anyhow::anyhow!("missing runtime_name column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("runtime_name column has unexpected type"))?;
    let runtime_versions = batch
        .column_by_name("runtime_version")
        .ok_or_else(|| anyhow::anyhow!("missing runtime_version column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("runtime_version column has unexpected type"))?;
    let runtime_flags = batch
        .column_by_name("runtime_flags")
        .ok_or_else(|| anyhow::anyhow!("missing runtime_flags column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("runtime_flags column has unexpected type"))?;
    // Lenient: parquet written before this column existed reads back as
    // all-null (same pattern as the optional power-state columns above).
    let runtime_cpu_variant_col = batch
        .column_by_name("runtime_cpu_variant")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("runtime_cpu_variant column has unexpected type"))
        })
        .transpose()?;
    let values = batch
        .column_by_name("value")
        .ok_or_else(|| anyhow::anyhow!("missing value column"))?
        .as_any()
        .downcast_ref::<Float32Array>()
        .ok_or_else(|| anyhow::anyhow!("value column has unexpected type"))?;
    let value_stddevs = batch
        .column_by_name("value_stddev")
        .map(|column| {
            column
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| anyhow::anyhow!("value_stddev column has unexpected type"))
        })
        .transpose()?;
    let units = batch
        .column_by_name("unit")
        .ok_or_else(|| anyhow::anyhow!("missing unit column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("unit column has unexpected type"))?;
    let submitted_ats = batch
        .column_by_name("submitted_at")
        .ok_or_else(|| anyhow::anyhow!("missing submitted_at column"))?
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .ok_or_else(|| anyhow::anyhow!("submitted_at column has unexpected type"))?;
    let scored_ats = batch
        .column_by_name("scored_at")
        .ok_or_else(|| anyhow::anyhow!("missing scored_at column"))?
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .ok_or_else(|| anyhow::anyhow!("scored_at column has unexpected type"))?;
    let prefill_tokens = batch
        .column_by_name("parameter_prefill_tokens")
        .ok_or_else(|| anyhow::anyhow!("missing parameter_prefill_tokens column"))?
        .as_any()
        .downcast_ref::<Int32Array>()
        .ok_or_else(|| anyhow::anyhow!("parameter_prefill_tokens column has unexpected type"))?;
    let decode_tokens = batch
        .column_by_name("parameter_decode_tokens")
        .ok_or_else(|| anyhow::anyhow!("missing parameter_decode_tokens column"))?
        .as_any()
        .downcast_ref::<Int32Array>()
        .ok_or_else(|| anyhow::anyhow!("parameter_decode_tokens column has unexpected type"))?;
    let eval_ids = batch
        .column_by_name("parameter_eval_id")
        .ok_or_else(|| anyhow::anyhow!("missing parameter_eval_id column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("parameter_eval_id column has unexpected type"))?;
    let image_widths = batch
        .column_by_name("parameter_image_width")
        .map(|c| {
            c.as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| anyhow::anyhow!("parameter_image_width column has unexpected type"))
        })
        .transpose()?;
    let image_heights = batch
        .column_by_name("parameter_image_height")
        .map(|c| {
            c.as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| anyhow::anyhow!("parameter_image_height column has unexpected type"))
        })
        .transpose()?;
    let text_tokens = batch
        .column_by_name("parameter_text_tokens")
        .map(|c| {
            c.as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| anyhow::anyhow!("parameter_text_tokens column has unexpected type"))
        })
        .transpose()?;
    // Lenient: absent in pre-feature parquet, reads back as all-null.
    let num_images = batch
        .column_by_name("parameter_num_images")
        .map(|c| {
            c.as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| anyhow::anyhow!("parameter_num_images column has unexpected type"))
        })
        .transpose()?;
    // Lenient: absent in pre-feature parquet, reads back as all-null.
    let obs_prefill_tokens = batch
        .column_by_name("observation_vl_throughput_prefill_tokens")
        .map(|c| {
            c.as_any().downcast_ref::<Int32Array>().ok_or_else(|| {
                anyhow::anyhow!(
                    "observation_vl_throughput_prefill_tokens column has unexpected type"
                )
            })
        })
        .transpose()?;
    let obs_image_tokens = batch
        .column_by_name("observation_vl_throughput_image_tokens")
        .map(|c| {
            c.as_any().downcast_ref::<Int32Array>().ok_or_else(|| {
                anyhow::anyhow!("observation_vl_throughput_image_tokens column has unexpected type")
            })
        })
        .transpose()?;
    let score_runtime_versions = batch
        .column_by_name("score_runtime_version")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("score_runtime_version column has unexpected type"))
        })
        .transpose()?;
    // Optional column for pre-feature parquet files; absence treated as
    // None so old data reads back cleanly.
    let eval_metadatas = batch
        .column_by_name("eval_metadata")
        .map(|c| {
            c.as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow::anyhow!("eval_metadata column has unexpected type"))
        })
        .transpose()?;
    // Lenient: absent in pre-feature parquet, reads back as all-null.
    let obs_swap_bytes = batch
        .column_by_name("observation_max_swap_bytes")
        .map(|c| {
            c.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
                anyhow::anyhow!("observation_max_swap_bytes column has unexpected type")
            })
        })
        .transpose()?;
    // Lenient: absent in pre-feature parquet, reads back as all-null.
    let obs_host_bytes = batch
        .column_by_name("observation_max_host_bytes")
        .map(|c| {
            c.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
                anyhow::anyhow!("observation_max_host_bytes column has unexpected type")
            })
        })
        .transpose()?;

    let mut rows = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        rows.push(MetricRow {
            result_id: result_ids.value(i).to_string(),
            benchmark_id: BenchmarkId::try_new(benchmark_ids.value(i))?,
            benchmark_type: benchmark_types
                .value(i)
                .parse::<BenchmarkType>()
                .map_err(|e| anyhow::anyhow!("row {i}: {e}"))?,
            metric: metrics.value(i).to_string(),
            client_id: ClientId::try_new(client_ids.value(i))?,
            device_name: device_names.value(i).to_string(),
            device_form_factor: device_form_factors
                .value(i)
                .parse::<DeviceFormFactor>()
                .map_err(|e| anyhow::anyhow!("row {i}: {e}"))?,
            device_os_name: device_os_names.value(i).to_string(),
            device_os_version: device_os_versions.value(i).to_string(),
            device_os_build: device_os_build_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_os_security_patch: device_os_security_patch_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_chip_model: device_chip_models.value(i).to_string(),
            device_gpu_model: (!device_gpu_models.is_null(i))
                .then(|| device_gpu_models.value(i).to_string()),
            device_gpu_vram_bytes: (!device_gpu_vram_bytes.is_null(i))
                .then(|| device_gpu_vram_bytes.value(i)),
            device_npu_model: (!device_npu_models.is_null(i))
                .then(|| device_npu_models.value(i).to_string()),
            device_npu_vram_bytes: (!device_npu_vram_bytes.is_null(i))
                .then(|| device_npu_vram_bytes.value(i)),
            device_ram_bytes: device_ram_bytes.value(i),
            device_battery_level: device_battery_level_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i)))
                .map(BatteryLevel::try_new)
                .transpose()
                .map_err(|e| anyhow::anyhow!("row {i}: device_battery_level: {e}"))?,
            device_power_state: device_power_state_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i)))
                .map(|s| {
                    s.parse::<DevicePowerState>()
                        .map_err(|e| anyhow::anyhow!("row {i}: device_power_state: {e}"))
                })
                .transpose()?,
            device_power_save_mode: device_power_save_mode_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            device_android_cpuset: device_android_cpuset_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_android_cpu_affinity_list: device_android_cpu_affinity_list_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_android_cpu_affinity_excludes_top_tier:
                device_android_cpu_affinity_excludes_top_tier_col
                    .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            device_apple_thermal_state_before: device_apple_thermal_state_before_col[i].clone(),
            device_apple_thermal_state_after: device_apple_thermal_state_after_col[i].clone(),
            device_apple_soc_temp_c_before: device_apple_soc_temp_c_before_col[i].clone(),
            device_apple_soc_temp_c_after: device_apple_soc_temp_c_after_col[i].clone(),
            device_android_thermal_status_before: device_android_thermal_status_before_col[i]
                .clone(),
            device_android_thermal_status_after: device_android_thermal_status_after_col[i].clone(),
            device_android_thermal_headroom_before: device_android_thermal_headroom_before_col[i]
                .clone(),
            device_android_thermal_headroom_after: device_android_thermal_headroom_after_col[i]
                .clone(),
            device_android_thermal_sensors_before: device_android_thermal_sensors_before_col[i]
                .clone(),
            device_android_thermal_sensors_after: device_android_thermal_sensors_after_col[i]
                .clone(),
            device_linux_thermal_zones_before: device_linux_thermal_zones_before_col[i].clone(),
            device_linux_thermal_zones_after: device_linux_thermal_zones_after_col[i].clone(),
            model_name: (!model_names.is_null(i)).then(|| model_names.value(i).to_string()),
            model_quant: (!model_quants.is_null(i)).then(|| model_quants.value(i).to_string()),
            model_params_total_millions: model_params_total_millions_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            model_params_active_millions: model_params_active_millions_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            model_flags: (!model_flags.is_null(i)).then(|| model_flags.value(i).to_string()),
            runtime_name: (!runtime_names.is_null(i)).then(|| runtime_names.value(i).to_string()),
            runtime_version: (!runtime_versions.is_null(i))
                .then(|| runtime_versions.value(i).to_string()),
            runtime_flags: (!runtime_flags.is_null(i)).then(|| runtime_flags.value(i).to_string()),
            runtime_cpu_variant: runtime_cpu_variant_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            value: values.value(i),
            value_stddev: value_stddevs.and_then(|value_stddevs| {
                (!value_stddevs.is_null(i)).then(|| value_stddevs.value(i))
            }),
            unit: units.value(i).to_string(),
            submitted_at: submitted_ats.value(i),
            scored_at: scored_ats.value(i),
            parameter_prefill_tokens: (!prefill_tokens.is_null(i)).then(|| prefill_tokens.value(i)),
            parameter_decode_tokens: (!decode_tokens.is_null(i)).then(|| decode_tokens.value(i)),
            parameter_eval_id: (!eval_ids.is_null(i)).then(|| eval_ids.value(i).to_string()),
            parameter_image_width: image_widths.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_image_height: image_heights.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_text_tokens: text_tokens.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_num_images: num_images.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_vl_throughput_prefill_tokens: obs_prefill_tokens
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_vl_throughput_image_tokens: obs_image_tokens
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            score_runtime_version: score_runtime_versions
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            eval_metadata: eval_metadatas
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            model_descriptor: model_descriptors
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            runtime_descriptor: runtime_descriptors
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            model_descriptor_sha256: model_descriptor_sha256s
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            runtime_descriptor_sha256: runtime_descriptor_sha256s
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            benchmark_flags: benchmark_flags_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            benchmark_flags_sha256: benchmark_flags_sha256s
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            client_version: client_versions
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            model_flags_sha256: model_flags_sha256s
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            runtime_flags_sha256: runtime_flags_sha256s
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            observation_max_swap_bytes: obs_swap_bytes
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_max_host_bytes: obs_host_bytes
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
        });
    }

    Ok(rows)
}

fn rows_to_batch(schema: &Arc<Schema>, rows: &[MetricRow]) -> anyhow::Result<RecordBatch> {
    let result_ids: Vec<&str> = rows.iter().map(|r| r.result_id.as_str()).collect();
    let benchmark_ids: Vec<&str> = rows.iter().map(|r| r.benchmark_id.as_str()).collect();
    let benchmark_types: Vec<&str> = rows.iter().map(|r| r.benchmark_type.as_ref()).collect();
    let metrics: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
    let client_ids: Vec<&str> = rows.iter().map(|r| r.client_id.as_str()).collect();
    let device_names: Vec<&str> = rows.iter().map(|r| r.device_name.as_str()).collect();
    let device_form_factors: Vec<&str> =
        rows.iter().map(|r| r.device_form_factor.as_ref()).collect();
    let device_os_names: Vec<&str> = rows.iter().map(|r| r.device_os_name.as_str()).collect();
    let device_os_versions: Vec<&str> = rows.iter().map(|r| r.device_os_version.as_str()).collect();
    let device_os_builds: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_os_build.as_deref()).collect();
    let device_os_security_patches: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_os_security_patch.as_deref())
        .collect();
    let device_chip_models: Vec<&str> = rows.iter().map(|r| r.device_chip_model.as_str()).collect();
    let device_gpu_models: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_gpu_model.as_deref()).collect();
    let device_gpu_vram_bytes_col: Vec<Option<i64>> =
        rows.iter().map(|r| r.device_gpu_vram_bytes).collect();
    let device_npu_models: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_npu_model.as_deref()).collect();
    let device_npu_vram_bytes_col: Vec<Option<i64>> =
        rows.iter().map(|r| r.device_npu_vram_bytes).collect();
    let device_ram_bytes_col: Vec<i64> = rows.iter().map(|r| r.device_ram_bytes).collect();
    let device_battery_level_col: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.device_battery_level.map(i32::from))
        .collect();
    let device_power_state_col: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_power_state.as_ref().map(|s| s.as_ref()))
        .collect();
    let device_power_save_mode_col: Vec<Option<bool>> =
        rows.iter().map(|r| r.device_power_save_mode).collect();
    let device_android_cpuset_col: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_android_cpuset.as_deref())
        .collect();
    let device_android_cpu_affinity_list_col: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_android_cpu_affinity_list.as_deref())
        .collect();
    let device_android_cpu_affinity_excludes_top_tier_col: Vec<Option<bool>> = rows
        .iter()
        .map(|r| r.device_android_cpu_affinity_excludes_top_tier)
        .collect();
    // Per-iteration thermal series. Scalar families become `Vec<Option<Vec<&str>>>`
    // / `Vec<Option<Vec<f32>>>` (one inner element per repetition); sensor/zone
    // families pass through as owned `Vec`s flattened by the list builders.
    let enum_series = |get: &dyn Fn(&MetricRow) -> Option<Vec<&str>>| -> Vec<Option<Vec<&str>>> {
        rows.iter().map(get).collect()
    };
    let device_apple_thermal_state_before_col = enum_series(&|r| {
        r.device_apple_thermal_state_before
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_ref()).collect())
    });
    let device_apple_thermal_state_after_col = enum_series(&|r| {
        r.device_apple_thermal_state_after
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_ref()).collect())
    });
    let device_apple_soc_temp_c_before_col: Vec<Option<Vec<f32>>> = rows
        .iter()
        .map(|r| r.device_apple_soc_temp_c_before.clone())
        .collect();
    let device_apple_soc_temp_c_after_col: Vec<Option<Vec<f32>>> = rows
        .iter()
        .map(|r| r.device_apple_soc_temp_c_after.clone())
        .collect();
    let device_android_thermal_status_before_col = enum_series(&|r| {
        r.device_android_thermal_status_before
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_ref()).collect())
    });
    let device_android_thermal_status_after_col = enum_series(&|r| {
        r.device_android_thermal_status_after
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_ref()).collect())
    });
    let device_android_thermal_headroom_before_col: Vec<Option<Vec<f32>>> = rows
        .iter()
        .map(|r| r.device_android_thermal_headroom_before.clone())
        .collect();
    let device_android_thermal_headroom_after_col: Vec<Option<Vec<f32>>> = rows
        .iter()
        .map(|r| r.device_android_thermal_headroom_after.clone())
        .collect();
    let device_android_thermal_sensors_before_col: Vec<Option<Vec<AndroidTemperatureSensor>>> =
        rows.iter()
            .map(|r| r.device_android_thermal_sensors_before.clone())
            .collect();
    let device_android_thermal_sensors_after_col: Vec<Option<Vec<AndroidTemperatureSensor>>> = rows
        .iter()
        .map(|r| r.device_android_thermal_sensors_after.clone())
        .collect();
    let device_linux_thermal_zones_before_col: Vec<Option<Vec<LinuxThermalZone>>> = rows
        .iter()
        .map(|r| r.device_linux_thermal_zones_before.clone())
        .collect();
    let device_linux_thermal_zones_after_col: Vec<Option<Vec<LinuxThermalZone>>> = rows
        .iter()
        .map(|r| r.device_linux_thermal_zones_after.clone())
        .collect();
    let model_names: Vec<Option<&str>> = rows.iter().map(|r| r.model_name.as_deref()).collect();
    let model_quants: Vec<Option<&str>> = rows.iter().map(|r| r.model_quant.as_deref()).collect();
    let model_params_total_millions_col: Vec<Option<i32>> =
        rows.iter().map(|r| r.model_params_total_millions).collect();
    let model_params_active_millions_col: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.model_params_active_millions)
        .collect();
    let model_descriptors: Vec<Option<&str>> =
        rows.iter().map(|r| r.model_descriptor.as_deref()).collect();
    let runtime_descriptors: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.runtime_descriptor.as_deref())
        .collect();
    let model_descriptor_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.model_descriptor_sha256.as_deref())
        .collect();
    let runtime_descriptor_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.runtime_descriptor_sha256.as_deref())
        .collect();
    let benchmark_flags_col: Vec<Option<&str>> =
        rows.iter().map(|r| r.benchmark_flags.as_deref()).collect();
    let benchmark_flags_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.benchmark_flags_sha256.as_deref())
        .collect();
    let client_versions: Vec<Option<&str>> =
        rows.iter().map(|r| r.client_version.as_deref()).collect();
    let model_flags_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.model_flags_sha256.as_deref())
        .collect();
    let runtime_flags_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.runtime_flags_sha256.as_deref())
        .collect();
    let model_flags: Vec<Option<&str>> = rows.iter().map(|r| r.model_flags.as_deref()).collect();
    let runtime_names: Vec<Option<&str>> = rows.iter().map(|r| r.runtime_name.as_deref()).collect();
    let runtime_versions: Vec<Option<&str>> =
        rows.iter().map(|r| r.runtime_version.as_deref()).collect();
    let runtime_flags: Vec<Option<&str>> =
        rows.iter().map(|r| r.runtime_flags.as_deref()).collect();
    let runtime_cpu_variant_col: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.runtime_cpu_variant.as_deref())
        .collect();
    let values: Vec<f32> = rows.iter().map(|r| r.value).collect();
    let value_stddevs: Vec<Option<f32>> = rows.iter().map(|r| r.value_stddev).collect();
    let units: Vec<&str> = rows.iter().map(|r| r.unit.as_str()).collect();
    let submitted_ats: Vec<i64> = rows.iter().map(|r| r.submitted_at).collect();
    let scored_ats: Vec<i64> = rows.iter().map(|r| r.scored_at).collect();
    let prefill_tokens: Vec<Option<i32>> =
        rows.iter().map(|r| r.parameter_prefill_tokens).collect();
    let decode_tokens: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_decode_tokens).collect();
    let eval_ids: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.parameter_eval_id.as_deref())
        .collect();
    let image_widths: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_image_width).collect();
    let image_heights: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_image_height).collect();
    let text_tokens: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_text_tokens).collect();
    let num_images: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_num_images).collect();
    let obs_prefill_tokens: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.observation_vl_throughput_prefill_tokens)
        .collect();
    let obs_image_tokens: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.observation_vl_throughput_image_tokens)
        .collect();
    let score_runtime_versions: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.score_runtime_version.as_deref())
        .collect();
    let eval_metadatas: Vec<Option<&str>> =
        rows.iter().map(|r| r.eval_metadata.as_deref()).collect();
    let obs_swap_bytes: Vec<Option<i64>> =
        rows.iter().map(|r| r.observation_max_swap_bytes).collect();
    let obs_host_bytes: Vec<Option<i64>> =
        rows.iter().map(|r| r.observation_max_host_bytes).collect();

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(result_ids)),
            Arc::new(StringArray::from(benchmark_ids)),
            Arc::new(StringArray::from(benchmark_types)),
            Arc::new(StringArray::from(metrics)),
            Arc::new(StringArray::from(client_ids)),
            Arc::new(StringArray::from(device_names)),
            Arc::new(StringArray::from(device_form_factors)),
            Arc::new(StringArray::from(device_os_names)),
            Arc::new(StringArray::from(device_os_versions)),
            Arc::new(StringArray::from(device_os_builds)),
            Arc::new(StringArray::from(device_os_security_patches)),
            Arc::new(StringArray::from(device_chip_models)),
            Arc::new(StringArray::from(device_gpu_models)),
            Arc::new(Int64Array::from(device_gpu_vram_bytes_col)),
            Arc::new(StringArray::from(device_npu_models)),
            Arc::new(Int64Array::from(device_npu_vram_bytes_col)),
            Arc::new(Int64Array::from(device_ram_bytes_col)),
            Arc::new(Int32Array::from(device_battery_level_col)),
            Arc::new(StringArray::from(device_power_state_col)),
            Arc::new(BooleanArray::from(device_power_save_mode_col)),
            Arc::new(StringArray::from(device_android_cpuset_col)),
            Arc::new(StringArray::from(device_android_cpu_affinity_list_col)),
            Arc::new(BooleanArray::from(
                device_android_cpu_affinity_excludes_top_tier_col,
            )),
            build_string_list_array(&device_apple_thermal_state_before_col),
            build_string_list_array(&device_apple_thermal_state_after_col),
            build_f32_list_array(&device_apple_soc_temp_c_before_col),
            build_f32_list_array(&device_apple_soc_temp_c_after_col),
            build_string_list_array(&device_android_thermal_status_before_col),
            build_string_list_array(&device_android_thermal_status_after_col),
            build_f32_list_array(&device_android_thermal_headroom_before_col),
            build_f32_list_array(&device_android_thermal_headroom_after_col),
            build_android_sensors_array(&device_android_thermal_sensors_before_col),
            build_android_sensors_array(&device_android_thermal_sensors_after_col),
            build_linux_zones_array(&device_linux_thermal_zones_before_col),
            build_linux_zones_array(&device_linux_thermal_zones_after_col),
            Arc::new(StringArray::from(model_names)),
            Arc::new(StringArray::from(model_quants)),
            Arc::new(Int32Array::from(model_params_total_millions_col)),
            Arc::new(Int32Array::from(model_params_active_millions_col)),
            Arc::new(StringArray::from(model_flags)),
            Arc::new(StringArray::from(runtime_names)),
            Arc::new(StringArray::from(runtime_versions)),
            Arc::new(StringArray::from(runtime_flags)),
            Arc::new(StringArray::from(runtime_cpu_variant_col)),
            Arc::new(Float32Array::from(values)),
            Arc::new(StringArray::from(units)),
            Arc::new(TimestampMicrosecondArray::from(submitted_ats).with_timezone("UTC")),
            Arc::new(TimestampMicrosecondArray::from(scored_ats).with_timezone("UTC")),
            Arc::new(Int32Array::from(prefill_tokens)),
            Arc::new(Int32Array::from(decode_tokens)),
            Arc::new(StringArray::from(eval_ids)),
            Arc::new(Float32Array::from(value_stddevs)),
            Arc::new(Int32Array::from(image_widths)),
            Arc::new(Int32Array::from(image_heights)),
            Arc::new(Int32Array::from(text_tokens)),
            Arc::new(StringArray::from(score_runtime_versions)),
            Arc::new(StringArray::from(eval_metadatas)),
            Arc::new(Int32Array::from(num_images)),
            Arc::new(Int32Array::from(obs_prefill_tokens)),
            Arc::new(Int32Array::from(obs_image_tokens)),
            Arc::new(StringArray::from(model_descriptors)),
            Arc::new(StringArray::from(runtime_descriptors)),
            Arc::new(StringArray::from(model_descriptor_sha256s)),
            Arc::new(StringArray::from(runtime_descriptor_sha256s)),
            Arc::new(StringArray::from(benchmark_flags_col)),
            Arc::new(StringArray::from(benchmark_flags_sha256s)),
            Arc::new(StringArray::from(client_versions)),
            Arc::new(StringArray::from(model_flags_sha256s)),
            Arc::new(StringArray::from(runtime_flags_sha256s)),
            Arc::new(Int64Array::from(obs_swap_bytes)),
            Arc::new(Int64Array::from(obs_host_bytes)),
        ],
    )?;

    Ok(batch)
}

pub struct JobMetrics {
    pub scored_at: String, // ISO 8601
    pub score_runtime_version: Option<String>,
    pub metrics: Vec<JobMetric>,
}

impl JobMetrics {
    /// Build from a set of `MetricRow`s belonging to a single job.
    /// Returns `None` if `rows` is empty.
    /// Build a job's metrics from the **latest scoring run** among `rows`.
    ///
    /// Append-only writes can leave several copies of a job's rows: a crash
    /// before `mark_processed` re-scores the job and appends a fresh copy
    /// rather than replacing the old one. All rows of one scoring run share a
    /// `scored_at`, so the newest copy is exactly the rows with the maximum
    /// `scored_at` — earlier copies are dropped **wholesale**, so a re-score
    /// that produces a different metric set can't leave stale rows behind.
    /// Order-independent, so callers needn't sort parts.
    pub(crate) fn from_latest_rows(rows: &[MetricRow]) -> Option<Self> {
        let latest_scored_at = rows.iter().map(|r| r.scored_at).max()?;
        let latest: Vec<&MetricRow> = rows
            .iter()
            .filter(|r| r.scored_at == latest_scored_at)
            .collect();
        let scored_at = chrono::DateTime::from_timestamp_micros(latest_scored_at)?.to_rfc3339();
        let metrics = latest
            .iter()
            .map(|r| JobMetric {
                metric: r.metric.clone(),
                value: r.value,
                value_stddev: r.value_stddev,
                unit: r.unit.clone(),
            })
            .collect();
        Some(Self {
            scored_at,
            score_runtime_version: latest.first().and_then(|r| r.score_runtime_version.clone()),
            metrics,
        })
    }
}

#[derive(serde::Serialize)]
pub struct JobMetric {
    pub metric: String,
    pub value: f32,
    pub value_stddev: Option<f32>,
    pub unit: String,
}

/// Read metrics for a specific job from Parquet files.
///
/// Partitions are scanned within the last `read_days` (a hard cap — a job
/// older than that is not found here; callers report it without metrics),
/// `day=` first then legacy `month=` (see `select_partitions_to_scan`).
/// A job almost always lives in one partition; in the rare cross-cutover
/// case it can be in both a `day=` and a same-period `month=`, but days
/// are scanned first, so the **first partition that matches is the newest**
/// — the scan stops there, keeping a recent-job lookup at ~one partition.
///
/// The job's rows from that partition are passed to
/// [`JobMetrics::from_latest_rows`], which returns only the newest scoring
/// run (max `scored_at`), so an append-only crash-retry duplicate resolves
/// to its latest copy with no on-disk dedup.
pub fn read_metrics_for_job(
    warehouse_dir: &Path,
    benchmark_id: &BenchmarkId,
    client_id: &ClientId,
    job_id: &JobId,
    read_days: u32,
) -> anyhow::Result<Option<JobMetrics>> {
    let partition_dir = warehouse_dir
        .join(format!("benchmark_id={benchmark_id}"))
        .join(format!("client_id={client_id}"));

    if !partition_dir.exists() {
        return Ok(None);
    }

    // Map each partition's Hive key ("day=YYYY-MM-DD" or legacy
    // "month=YYYY-MM") to its path, then window + order via the shared
    // selector (newest first).
    let mut by_key: std::collections::HashMap<String, std::path::PathBuf> =
        std::collections::HashMap::new();
    for entry in std::fs::read_dir(&partition_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(key) = entry.file_name().to_str()
        {
            by_key.insert(key.to_string(), entry.path());
        }
    }
    let today = chrono::Utc::now().date_naive();
    let selected = select_partitions_to_scan(by_key.keys().cloned(), read_days, today);

    let prefix = format!("{job_id}_");
    for key in &selected {
        let Some(partition_path) = by_key.get(key) else {
            continue;
        };
        // Gather the job's rows across this partition's part files; order is
        // irrelevant because `from_latest_rows` selects the newest copy by
        // `scored_at`. Collecting into a `Result` propagates a read error
        // rather than dropping an entry.
        let matching: Vec<MetricRow> = std::fs::read_dir(partition_path)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "parquet"))
            .map(|p| read_part_rows(&p))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .filter(|r| r.result_id.starts_with(&prefix))
            .collect();
        // Partitions are scanned newest-first (days before months), so the
        // first one with a match holds the newest copy; stop there.
        if !matching.is_empty() {
            return Ok(JobMetrics::from_latest_rows(&matching));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use crate::warehouse::*;
    use anyhow::Context;
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use rstest::rstest;
    use strum::IntoEnumIterator;

    fn make_test_row(metric: &str, value: f32, unit: &str) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            metric: metric.to_string(),
            value,
            unit: unit.to_string(),
            submitted_at: 1_000_000,
            scored_at: 2_000_000,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    #[test]
    fn test_device_form_factor_from_str() -> anyhow::Result<()> {
        for v in DeviceFormFactor::iter() {
            assert_eq!(
                v.as_ref()
                    .parse::<DeviceFormFactor>()
                    .map_err(|e: strum::ParseError| anyhow::anyhow!(e))?,
                v
            );
        }
        assert!("spaceship".parse::<DeviceFormFactor>().is_err());
        assert!("Phone".parse::<DeviceFormFactor>().is_err());
        assert!("EMBEDDED".parse::<DeviceFormFactor>().is_err());
        assert!("".parse::<DeviceFormFactor>().is_err());
        Ok(())
    }

    #[test]
    fn test_write_and_read_parquet() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let rows = vec![
            make_test_row("ttft", 34.7, "ms")?,
            make_test_row("prefill_throughput", 7378.1, "tokens/sec")?,
        ];

        append_to_parquet(WriterOpts::default(), &path, &rows)?;

        // Read back
        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let mut total = 0;
        for batch in reader {
            total += batch?.num_rows();
        }
        assert_eq!(total, 2);
        Ok(())
    }

    #[test]
    fn test_append_to_existing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let rows1 = vec![make_test_row("ttft", 34.7, "ms")?];
        append_to_parquet(WriterOpts::default(), &path, &rows1)?;

        let rows2 = vec![make_test_row("prefill_throughput", 7378.1, "tokens/sec")?];
        append_to_parquet(WriterOpts::default(), &path, &rows2)?;

        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let mut total = 0;
        for batch in reader {
            total += batch?.num_rows();
        }
        assert_eq!(total, 2);
        Ok(())
    }

    #[test]
    fn test_gpu_npu_fields_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let mut row = make_test_row("ttft", 34.7, "ms")?;
        row.device_gpu_model = Some("NVIDIA RTX 4090".to_string());
        row.device_gpu_vram_bytes = Some(25_769_803_776);
        row.device_npu_model = Some("Hailo-8 26T".to_string());
        row.device_npu_vram_bytes = None; // Hailo has no reportable VRAM

        append_to_parquet(WriterOpts::default(), &path, &[row])?;

        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let mut rows = Vec::new();
        for batch in reader {
            rows.extend(batch_to_rows(&batch?)?);
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_gpu_model.as_deref(), Some("NVIDIA RTX 4090"));
        assert_eq!(rows[0].device_gpu_vram_bytes, Some(25_769_803_776));
        assert_eq!(rows[0].device_npu_model.as_deref(), Some("Hailo-8 26T"));
        assert_eq!(rows[0].device_npu_vram_bytes, None);
        assert_eq!(rows[0].device_ram_bytes, 17_179_869_184);
        assert_eq!(rows[0].device_chip_model, "test-chip");
        assert_eq!(rows[0].device_form_factor, DeviceFormFactor::Embedded);
        Ok(())
    }

    #[test]
    fn test_os_build_security_patch_round_trip() -> anyhow::Result<()> {
        // Row 0 populates both fields; row 1 leaves them `None` so the
        // `Some`/`None` split survives the array round-trip.
        let mut populated = make_test_row("ttft", 34.7, "ms")?;
        populated.device_os_build = Some("AP3A.240905.015.A2".to_string());
        populated.device_os_security_patch = Some("2025-06-01".to_string());

        let empty = make_test_row("ttft", 30.1, "ms")?;
        assert_eq!(empty.device_os_build, None);
        assert_eq!(empty.device_os_security_patch, None);

        let rows = round_trip(&[populated, empty])?;
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].device_os_build.as_deref(),
            Some("AP3A.240905.015.A2")
        );
        assert_eq!(
            rows[0].device_os_security_patch.as_deref(),
            Some("2025-06-01")
        );
        assert_eq!(rows[1].device_os_build, None);
        assert_eq!(rows[1].device_os_security_patch, None);
        Ok(())
    }

    /// Write `rows` to a fresh parquet file and read every row back
    /// through `batch_to_rows` — the write→read round-trip shared by the
    /// parquet tests.
    fn round_trip(rows: &[MetricRow]) -> anyhow::Result<Vec<MetricRow>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");
        append_to_parquet(WriterOpts::default(), &path, rows)?;

        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        Ok(reader
            .map(|batch| batch_to_rows(&batch?))
            .collect::<anyhow::Result<Vec<_>>>()?
            .concat())
    }

    #[test]
    fn test_battery_power_fields_round_trip() -> anyhow::Result<()> {
        // Three rows so all three `DevicePowerState` variants, both
        // `device_power_save_mode` booleans, and `Some`/`None` for the optional
        // int + `runtime_cpu_variant` are exercised through the array
        // round-trip. The all-`None` (missing) case is covered by the default
        // round-trip test.
        let mut on_battery = make_test_row("ttft", 34.7, "ms")?;
        on_battery.device_battery_level = Some(BatteryLevel::try_new(42)?);
        on_battery.device_power_state = Some(DevicePowerState::NotCharging);
        on_battery.device_power_save_mode = Some(true);
        on_battery.runtime_cpu_variant = Some("armv8.2_1".to_string());

        let mut charging = make_test_row("ttft", 30.1, "ms")?;
        charging.device_battery_level = Some(BatteryLevel::try_new(100)?);
        charging.device_power_state = Some(DevicePowerState::Charging);
        charging.device_power_save_mode = Some(false);
        charging.runtime_cpu_variant = None;

        let mut plugged_full = make_test_row("ttft", 31.5, "ms")?;
        plugged_full.device_battery_level = Some(BatteryLevel::try_new(100)?);
        plugged_full.device_power_state = Some(DevicePowerState::PluggedInNotCharging);
        plugged_full.device_power_save_mode = Some(false);
        plugged_full.runtime_cpu_variant = Some("armv8.6_1".to_string());

        let rows = round_trip(&[on_battery, charging, plugged_full])?;
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].device_battery_level,
            Some(BatteryLevel::try_new(42)?)
        );
        assert_eq!(
            rows[0].device_power_state,
            Some(DevicePowerState::NotCharging)
        );
        assert_eq!(rows[0].device_power_save_mode, Some(true));
        assert_eq!(rows[0].runtime_cpu_variant.as_deref(), Some("armv8.2_1"));
        assert_eq!(rows[1].device_power_state, Some(DevicePowerState::Charging));
        assert_eq!(rows[1].runtime_cpu_variant, None);
        assert_eq!(
            rows[2].device_power_state,
            Some(DevicePowerState::PluggedInNotCharging)
        );
        assert_eq!(rows[2].device_power_save_mode, Some(false));
        assert_eq!(rows[2].runtime_cpu_variant.as_deref(), Some("armv8.6_1"));
        Ok(())
    }

    #[test]
    fn test_battery_power_fields_default_none_round_trip() -> anyhow::Result<()> {
        // A row that never set the fields reads back as all-None.
        let rows = round_trip(&[make_test_row("ttft", 34.7, "ms")?])?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_battery_level, None);
        assert_eq!(rows[0].device_power_state, None);
        assert_eq!(rows[0].device_power_save_mode, None);
        assert_eq!(rows[0].runtime_cpu_variant, None);
        assert_eq!(rows[0].device_android_cpuset, None);
        assert_eq!(rows[0].device_android_cpu_affinity_list, None);
        assert_eq!(rows[0].device_android_cpu_affinity_excludes_top_tier, None);
        Ok(())
    }

    #[test]
    fn test_android_cpuset_fields_round_trip() -> anyhow::Result<()> {
        // Two rows: a Samsung-style demotion (barred from the prime tier) and a
        // Pixel-style row that keeps every core, so both boolean states and the
        // string columns survive the parquet array round-trip.
        let mut demoted = make_test_row("ttft", 34.7, "ms")?;
        demoted.device_android_cpuset = Some("/moderate".to_string());
        demoted.device_android_cpu_affinity_list = Some("0-5".to_string());
        demoted.device_android_cpu_affinity_excludes_top_tier = Some(true);

        let mut full = make_test_row("ttft", 30.1, "ms")?;
        full.device_android_cpuset = Some("/top-app".to_string());
        full.device_android_cpu_affinity_list = Some("0-7".to_string());
        full.device_android_cpu_affinity_excludes_top_tier = Some(false);

        let rows = round_trip(&[demoted, full])?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].device_android_cpuset.as_deref(), Some("/moderate"));
        assert_eq!(
            rows[0].device_android_cpu_affinity_list.as_deref(),
            Some("0-5")
        );
        assert_eq!(
            rows[0].device_android_cpu_affinity_excludes_top_tier,
            Some(true)
        );
        assert_eq!(rows[1].device_android_cpuset.as_deref(), Some("/top-app"));
        assert_eq!(
            rows[1].device_android_cpu_affinity_list.as_deref(),
            Some("0-7")
        );
        assert_eq!(
            rows[1].device_android_cpu_affinity_excludes_top_tier,
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn test_num_images_round_trip() -> anyhow::Result<()> {
        // num_images round-trips as a nullable column: one row sets it, one doesn't.
        let mut multi_frame = make_test_row("prefill_throughput", 100.0, "tokens/sec")?;
        multi_frame.parameter_num_images = Some(80);
        let single = make_test_row("ttft", 34.7, "ms")?;
        let rows = round_trip(&[multi_frame, single])?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].parameter_num_images, Some(80));
        assert_eq!(rows[1].parameter_num_images, None);
        Ok(())
    }

    #[test]
    fn test_thermal_scalars_round_trip() -> anyhow::Result<()> {
        // A row carrying the per-iteration scalar thermal series round-trips
        // through the writer + reader, exercising the `List<Utf8>` enum columns
        // and the `List<Float32>` headroom columns across multiple reps.
        let mut hot = make_test_row("ttft", 34.7, "ms")?;
        hot.device_apple_thermal_state_before =
            Some(vec![AppleThermalState::Nominal, AppleThermalState::Fair]);
        hot.device_apple_thermal_state_after =
            Some(vec![AppleThermalState::Fair, AppleThermalState::Serious]);
        hot.device_android_thermal_status_before = Some(vec![
            AndroidThermalStatus::None,
            AndroidThermalStatus::Light,
        ]);
        hot.device_android_thermal_status_after = Some(vec![
            AndroidThermalStatus::Light,
            AndroidThermalStatus::Severe,
        ]);
        hot.device_android_thermal_headroom_before = Some(vec![0.31, 0.44]);
        hot.device_android_thermal_headroom_after = Some(vec![0.62, 0.71]);
        hot.device_apple_soc_temp_c_before = Some(vec![41.5, 44.25]);
        hot.device_apple_soc_temp_c_after = Some(vec![46.0, 49.75]);

        let rows = round_trip(&[hot])?;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].device_apple_thermal_state_before,
            Some(vec![AppleThermalState::Nominal, AppleThermalState::Fair])
        );
        assert_eq!(
            rows[0].device_apple_thermal_state_after,
            Some(vec![AppleThermalState::Fair, AppleThermalState::Serious])
        );
        assert_eq!(
            rows[0].device_android_thermal_status_before,
            Some(vec![
                AndroidThermalStatus::None,
                AndroidThermalStatus::Light
            ])
        );
        assert_eq!(
            rows[0].device_android_thermal_status_after,
            Some(vec![
                AndroidThermalStatus::Light,
                AndroidThermalStatus::Severe
            ])
        );
        assert_eq!(
            rows[0].device_android_thermal_headroom_before,
            Some(vec![0.31, 0.44])
        );
        assert_eq!(
            rows[0].device_android_thermal_headroom_after,
            Some(vec![0.62, 0.71])
        );
        assert_eq!(
            rows[0].device_apple_soc_temp_c_before,
            Some(vec![41.5, 44.25])
        );
        assert_eq!(
            rows[0].device_apple_soc_temp_c_after,
            Some(vec![46.0, 49.75])
        );
        Ok(())
    }

    #[test]
    fn test_vl_throughput_observation_columns_round_trip() -> anyhow::Result<()> {
        // The measured token counts round-trip as nullable observation
        // columns: a VL row carries them, a non-VL row leaves them None.
        let mut vl = make_test_row("ttft", 50.0, "ms")?;
        vl.observation_vl_throughput_prefill_tokens = Some(235);
        vl.observation_vl_throughput_image_tokens = Some(194);
        let non_vl = make_test_row("decode_throughput", 42.0, "tokens/sec")?;
        let rows = round_trip(&[vl, non_vl])?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].observation_vl_throughput_prefill_tokens, Some(235));
        assert_eq!(rows[0].observation_vl_throughput_image_tokens, Some(194));
        assert_eq!(rows[1].observation_vl_throughput_prefill_tokens, None);
        assert_eq!(rows[1].observation_vl_throughput_image_tokens, None);
        Ok(())
    }

    #[test]
    fn test_observed_memory_columns_round_trip() -> anyhow::Result<()> {
        // The per-run swap / host peaks round-trip as nullable `Int64` columns:
        // one row reports them, one leaves them None. Both values sit above
        // `2^32`, so a narrowing to `Int32` would truncate them.
        let mut reported = make_test_row("ttft", 50.0, "ms")?;
        reported.observation_max_swap_bytes = Some(6_442_450_944);
        reported.observation_max_host_bytes = Some(12_884_901_888);
        let silent = make_test_row("decode_throughput", 42.0, "tokens/sec")?;
        let rows = round_trip(&[reported, silent])?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].observation_max_swap_bytes, Some(6_442_450_944));
        assert_eq!(rows[0].observation_max_host_bytes, Some(12_884_901_888));
        assert_eq!(rows[1].observation_max_swap_bytes, None);
        assert_eq!(rows[1].observation_max_host_bytes, None);
        Ok(())
    }

    #[test]
    fn test_thermal_arrays_round_trip() -> anyhow::Result<()> {
        // The `List<Struct>` sensor + zone columns round-trip losslessly:
        // length, each element's fields, the `iteration` tag, the `celsius`
        // values, and the throttling-status enum all survive. The list
        // flattens two reps' worth of readings.
        let mut hot = make_test_row("ttft", 34.7, "ms")?;
        hot.device_android_thermal_sensors_before = Some(vec![
            AndroidTemperatureSensor {
                iteration: 0,
                sensor_type: "cpu".to_string(),
                name: "cpu-big".to_string(),
                celsius: 41,
                throttling_status: AndroidThrottlingSeverity::Light,
            },
            AndroidTemperatureSensor {
                iteration: 0,
                sensor_type: "battery".to_string(),
                name: "batt".to_string(),
                celsius: 33,
                throttling_status: AndroidThrottlingSeverity::None,
            },
            AndroidTemperatureSensor {
                iteration: 1,
                sensor_type: "cpu".to_string(),
                name: "cpu-big".to_string(),
                celsius: 48,
                throttling_status: AndroidThrottlingSeverity::Severe,
            },
        ]);
        hot.device_linux_thermal_zones_after = Some(vec![
            LinuxThermalZone {
                iteration: 0,
                zone_type: "x86_pkg_temp".to_string(),
                celsius: 58,
            },
            LinuxThermalZone {
                iteration: 1,
                zone_type: "x86_pkg_temp".to_string(),
                celsius: 63,
            },
        ]);

        let rows = round_trip(&[hot])?;
        assert_eq!(rows.len(), 1);

        let sensors = rows[0]
            .device_android_thermal_sensors_before
            .as_ref()
            .context("expected sensors")?;
        assert_eq!(sensors.len(), 3);
        assert_eq!(sensors[0].iteration, 0);
        assert_eq!(sensors[0].sensor_type, "cpu");
        assert_eq!(sensors[0].name, "cpu-big");
        assert_eq!(sensors[0].celsius, 41);
        assert_eq!(
            sensors[0].throttling_status,
            AndroidThrottlingSeverity::Light
        );
        assert_eq!(sensors[1].sensor_type, "battery");
        assert_eq!(sensors[1].celsius, 33);
        assert_eq!(sensors[2].iteration, 1);
        assert_eq!(sensors[2].celsius, 48);
        assert_eq!(
            sensors[2].throttling_status,
            AndroidThrottlingSeverity::Severe
        );
        // The `_after` sensor column was never set → None.
        assert_eq!(rows[0].device_android_thermal_sensors_after, None);

        let zones = rows[0]
            .device_linux_thermal_zones_after
            .as_ref()
            .context("expected zones")?;
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].iteration, 0);
        assert_eq!(zones[0].zone_type, "x86_pkg_temp");
        assert_eq!(zones[0].celsius, 58);
        assert_eq!(zones[1].iteration, 1);
        assert_eq!(zones[1].celsius, 63);
        assert_eq!(rows[0].device_linux_thermal_zones_before, None);
        Ok(())
    }

    #[test]
    fn test_thermal_default_none_round_trip() -> anyhow::Result<()> {
        // A row that never set any thermal field reads back with all
        // per-platform per-iteration columns (scalar series and lists) as None.
        let rows = round_trip(&[make_test_row("ttft", 34.7, "ms")?])?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_apple_thermal_state_before, None);
        assert_eq!(rows[0].device_apple_thermal_state_after, None);
        assert_eq!(rows[0].device_apple_soc_temp_c_before, None);
        assert_eq!(rows[0].device_apple_soc_temp_c_after, None);
        assert_eq!(rows[0].device_android_thermal_status_before, None);
        assert_eq!(rows[0].device_android_thermal_status_after, None);
        assert_eq!(rows[0].device_android_thermal_headroom_before, None);
        assert_eq!(rows[0].device_android_thermal_headroom_after, None);
        assert_eq!(rows[0].device_android_thermal_sensors_before, None);
        assert_eq!(rows[0].device_android_thermal_sensors_after, None);
        assert_eq!(rows[0].device_linux_thermal_zones_before, None);
        assert_eq!(rows[0].device_linux_thermal_zones_after, None);
        Ok(())
    }

    fn make_test_row_with_ids(
        result_id: &str,
        benchmark_id: &str,
        client_id: &str,
        metric: &str,
        value: f32,
        unit: &str,
        scored_at: i64,
    ) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: result_id.to_string(),
            benchmark_id: BenchmarkId::try_new(benchmark_id)?,
            metric: metric.to_string(),
            client_id: ClientId::try_new(client_id)?,
            value,
            unit: unit.to_string(),
            submitted_at: 1_000_000,
            scored_at,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    #[test]
    fn test_append_to_file_missing_newer_nullable_columns() -> anyhow::Result<()> {
        // Simulates appending to a file written with a schema that is
        // missing some newer nullable columns (value_stddev,
        // score_runtime_version, model_descriptor, VL parameter columns,
        // device_os_build, device_os_security_patch).
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let old_schema = Arc::new(Schema::new(vec![
            Field::new("result_id", DataType::Utf8, false),
            Field::new("benchmark_id", DataType::Utf8, false),
            Field::new("benchmark_type", DataType::Utf8, false),
            Field::new("metric", DataType::Utf8, false),
            Field::new("client_id", DataType::Utf8, false),
            Field::new("device_name", DataType::Utf8, false),
            Field::new("device_form_factor", DataType::Utf8, false),
            Field::new("device_os_name", DataType::Utf8, false),
            Field::new("device_os_version", DataType::Utf8, false),
            Field::new("device_chip_model", DataType::Utf8, false),
            Field::new("device_gpu_model", DataType::Utf8, true),
            Field::new("device_gpu_vram_bytes", DataType::Int64, true),
            Field::new("device_npu_model", DataType::Utf8, true),
            Field::new("device_npu_vram_bytes", DataType::Int64, true),
            Field::new("device_ram_bytes", DataType::Int64, false),
            Field::new("model_name", DataType::Utf8, false),
            Field::new("model_quant", DataType::Utf8, false),
            Field::new("model_flags", DataType::Utf8, true),
            Field::new("runtime_name", DataType::Utf8, false),
            Field::new("runtime_version", DataType::Utf8, false),
            Field::new("runtime_flags", DataType::Utf8, true),
            Field::new("value", DataType::Float32, false),
            Field::new("unit", DataType::Utf8, false),
            Field::new(
                "submitted_at",
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                false,
            ),
            Field::new(
                "scored_at",
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                false,
            ),
            Field::new("parameter_prefill_tokens", DataType::Int32, true),
            Field::new("parameter_decode_tokens", DataType::Int32, true),
            Field::new("parameter_eval_id", DataType::Utf8, true),
        ]));
        let old_batch = RecordBatch::try_new(
            old_schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["job1_0"])),
                Arc::new(StringArray::from(vec!["bench1"])),
                Arc::new(StringArray::from(vec!["prefill_throughput"])),
                Arc::new(StringArray::from(vec!["ttft"])),
                Arc::new(StringArray::from(vec!["client1"])),
                Arc::new(StringArray::from(vec!["test-device"])),
                Arc::new(StringArray::from(vec!["embedded"])),
                Arc::new(StringArray::from(vec!["Linux"])),
                Arc::new(StringArray::from(vec!["22.04"])),
                Arc::new(StringArray::from(vec!["test-chip"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(Int64Array::from(vec![None::<i64>])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(Int64Array::from(vec![None::<i64>])),
                Arc::new(Int64Array::from(vec![17_179_869_184i64])),
                Arc::new(StringArray::from(vec!["model"])),
                Arc::new(StringArray::from(vec!["q4"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(StringArray::from(vec!["rt"])),
                Arc::new(StringArray::from(vec!["v1"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(Float32Array::from(vec![34.7])),
                Arc::new(StringArray::from(vec!["ms"])),
                Arc::new(TimestampMicrosecondArray::from(vec![1_000_000]).with_timezone("UTC")),
                Arc::new(TimestampMicrosecondArray::from(vec![2_000_000]).with_timezone("UTC")),
                Arc::new(Int32Array::from(vec![Some(256)])),
                Arc::new(Int32Array::from(vec![None::<i32>])),
                Arc::new(StringArray::from(vec![None::<&str>])),
            ],
        )?;

        let file = std::fs::File::create(&path)?;
        let mut writer = ArrowWriter::try_new(file, old_schema, None)?;
        writer.write(&old_batch)?;
        writer.close()?;

        let mut new_row = make_test_row("prefill_throughput", 7378.1, "tokens/sec")?;
        new_row.result_id = "job1_1".to_string();
        new_row.value_stddev = Some(12.5);
        append_to_parquet(WriterOpts::default(), &path, &[new_row])?;

        let rows = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path)?)?
            .build()?
            .map(|b| batch_to_rows(&b?))
            .collect::<anyhow::Result<Vec<_>>>()?
            .concat();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value_stddev, None);
        assert_eq!(rows[1].value_stddev, Some(12.5));
        // The old file predates these columns; the lenient read yields `None`.
        assert_eq!(rows[0].device_os_build, None);
        assert_eq!(rows[0].device_os_security_patch, None);
        Ok(())
    }

    #[test]
    fn test_select_partitions_orders_and_windows() -> anyhow::Result<()> {
        use chrono::NaiveDate;
        let today =
            NaiveDate::from_ymd_opt(2026, 6, 20).ok_or_else(|| anyhow::anyhow!("valid date"))?;
        let keys = || {
            vec![
                "day=2026-06-20".to_string(),
                "day=2026-06-01".to_string(),
                "day=2026-05-31".to_string(), // same date as month=2026-05's last day
                "month=2026-05".to_string(),  // legacy; sorts by last day 2026-05-31
                "month=2026-03".to_string(),
                "not-a-partition".to_string(), // skipped
            ]
        };

        // 14-day window (cutoff 2026-06-06): only the same-fortnight day
        // partition; junk key dropped.
        assert_eq!(
            select_partitions_to_scan(keys(), 14, today),
            vec!["day=2026-06-20"]
        );

        // 30-day window (cutoff 2026-05-21): day partitions first (newest
        // first), then the overlapping legacy month; older month excluded.
        assert_eq!(
            select_partitions_to_scan(keys(), 30, today),
            vec![
                "day=2026-06-20",
                "day=2026-06-01",
                "day=2026-05-31",
                "month=2026-05"
            ]
        );

        // Wide window: every day partition first (newest first), then every
        // month partition. Note `day=2026-05-31` sorts before `month=2026-05`
        // despite the equal date — the day scheme wins the tie.
        assert_eq!(
            select_partitions_to_scan(keys(), 36_500, today),
            vec![
                "day=2026-06-20",
                "day=2026-06-01",
                "day=2026-05-31",
                "month=2026-05",
                "month=2026-03"
            ]
        );
        Ok(())
    }

    #[test]
    fn test_read_metrics_for_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse = dir.path().join("warehouse");
        let partition = warehouse
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let scored_at_micros = 1_700_000_000_000_000i64; // known timestamp

        let rows = vec![
            make_test_row_with_ids(
                "job1_0",
                "bench1",
                "client1",
                "ttft",
                34.7,
                "ms",
                scored_at_micros,
            )?,
            make_test_row_with_ids(
                "job1_1",
                "bench1",
                "client1",
                "prefill_throughput",
                7378.1,
                "tokens/sec",
                scored_at_micros,
            )?,
            make_test_row_with_ids(
                "job2_0",
                "bench1",
                "client1",
                "ttft",
                50.0,
                "ms",
                scored_at_micros,
            )?,
        ];
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &rows,
        )?;

        let result = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("job1"),
            36_500, // wide window covers the fixed-date fixture
        )?
        .context("expected job1 metrics")?;
        assert_eq!(result.metrics.len(), 2);
        assert_eq!(result.metrics[0].metric, "ttft");
        assert!((result.metrics[0].value - 34.7).abs() < 0.1);
        assert_eq!(result.metrics[1].metric, "prefill_throughput");
        assert!(!result.scored_at.is_empty());
        Ok(())
    }

    #[test]
    fn test_read_metrics_for_job_not_found() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse = dir.path().join("warehouse");
        let partition = warehouse
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let rows = vec![make_test_row_with_ids(
            "other_0", "bench1", "client1", "ttft", 34.7, "ms", 1_000_000,
        )?];
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &rows,
        )?;

        let result = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("nonexistent"),
            36_500, // wide window covers the fixed-date fixture
        )?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn test_read_metrics_for_job_across_multiple_part_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse = dir.path().join("warehouse");
        let partition = warehouse
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let scored_at_micros = 1_700_000_000_000_000i64;
        let rows1 = vec![make_test_row_with_ids(
            "job1_0",
            "bench1",
            "client1",
            "ttft",
            34.7,
            "ms",
            scored_at_micros,
        )?];
        let rows2 = vec![make_test_row_with_ids(
            "job1_1",
            "bench1",
            "client1",
            "prefill_throughput",
            7378.1,
            "tokens/sec",
            scored_at_micros,
        )?];

        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &rows1,
        )?;
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0002.parquet",
            &rows2,
        )?;

        let result = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("job1"),
            36_500, // wide window covers the fixed-date fixture
        )?
        .context("expected job1 metrics")?;
        assert_eq!(result.metrics.len(), 2);
        Ok(())
    }

    #[test]
    fn test_eval_metadata_round_trip_some_and_none() -> anyhow::Result<()> {
        // Verifies that a JSON-encoded `eval_metadata` blob (the
        // `{"samples_failed": N}` shape currently emitted by the eval
        // branch of `derive_metrics`) round-trips losslessly through the
        // warehouse parquet writer + reader, and that a `None` value on
        // a sibling row reads back as `None` rather than e.g. an empty
        // string.
        let dir = tempfile::tempdir()?;
        let warehouse = dir.path().join("warehouse");
        let partition = warehouse
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let scored_at_micros = 1_700_000_000_000_000i64;
        let mut row_with = make_test_row_with_ids(
            "job1_0",
            "bench1",
            "client1",
            "accuracy",
            0.5,
            "ratio",
            scored_at_micros,
        )?;
        row_with.eval_metadata = Some(r#"{"samples_failed":3}"#.to_string());
        let row_without = make_test_row_with_ids(
            "job2_0",
            "bench1",
            "client1",
            "accuracy",
            0.7,
            "ratio",
            scored_at_micros,
        )?;
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &[row_with, row_without],
        )?;

        let with_md = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("job1"),
            36_500, // wide window covers the fixed-date fixture
        )?
        .context("expected job1 metrics")?;
        let without_md = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("job2"),
            36_500, // wide window covers the fixed-date fixture
        )?
        .context("expected job2 metrics")?;

        // JobMetrics doesn't currently surface eval_metadata, so verify
        // round-trip by reading rows directly back from the partition.
        let rows = read_partition_rows(&partition)?;
        let with = rows
            .iter()
            .find(|r| r.result_id == "job1_0")
            .context("job1 row")?;
        let without = rows
            .iter()
            .find(|r| r.result_id == "job2_0")
            .context("job2 row")?;
        assert_eq!(
            with.eval_metadata.as_deref(),
            Some(r#"{"samples_failed":3}"#)
        );
        assert_eq!(without.eval_metadata, None);
        // Sanity: the JobMetrics read path still works for both rows.
        assert!(!with_md.metrics.is_empty());
        assert!(!without_md.metrics.is_empty());
        Ok(())
    }

    #[test]
    fn test_score_runtime_version_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse = dir.path().join("warehouse");
        let partition = warehouse
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let scored_at_micros = 1_700_000_000_000_000i64;
        let mut row = make_test_row_with_ids(
            "job1_0",
            "bench1",
            "client1",
            "accuracy",
            0.85,
            "ratio",
            scored_at_micros,
        )?;
        row.score_runtime_version = Some("v1.2.3".to_string());
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &[row],
        )?;

        let result = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("job1"),
            36_500, // wide window covers the fixed-date fixture
        )?
        .context("expected job1 metrics")?;
        assert_eq!(result.score_runtime_version.as_deref(), Some("v1.2.3"));
        Ok(())
    }

    #[test]
    fn test_score_runtime_version_none_when_absent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse = dir.path().join("warehouse");
        let partition = warehouse
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let scored_at_micros = 1_700_000_000_000_000i64;
        let row = make_test_row_with_ids(
            "job1_0",
            "bench1",
            "client1",
            "ttft",
            34.7,
            "ms",
            scored_at_micros,
        )?;
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &[row],
        )?;

        let result = read_metrics_for_job(
            &warehouse,
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("client1")?,
            &JobId::new_unchecked("job1"),
            36_500, // wide window covers the fixed-date fixture
        )?
        .context("expected job1 metrics")?;
        assert_eq!(result.score_runtime_version, None);
        Ok(())
    }

    #[test]
    fn test_model_params_round_trip_dense() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let mut row = make_test_row("ttft", 100.0, "ms")?;
        row.model_params_total_millions = Some(700);
        row.model_params_active_millions = Some(700);
        append_to_parquet(WriterOpts::default(), &path, &[row])?;

        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>()?;
        let rows = batch_to_rows(&batches[0])?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_params_total_millions, Some(700));
        assert_eq!(rows[0].model_params_active_millions, Some(700));
        Ok(())
    }

    #[test]
    fn test_model_params_round_trip_moe() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let mut row = make_test_row("ttft", 100.0, "ms")?;
        row.model_params_total_millions = Some(8340);
        row.model_params_active_millions = Some(1000);
        append_to_parquet(WriterOpts::default(), &path, &[row])?;

        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>()?;
        let rows = batch_to_rows(&batches[0])?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_params_total_millions, Some(8340));
        assert_eq!(rows[0].model_params_active_millions, Some(1000));
        Ok(())
    }

    #[test]
    fn test_model_params_none_when_absent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let row = make_test_row("ttft", 100.0, "ms")?;
        assert!(row.model_params_total_millions.is_none());
        assert!(row.model_params_active_millions.is_none());
        append_to_parquet(WriterOpts::default(), &path, &[row])?;

        let file = std::fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>()?;
        let rows = batch_to_rows(&batches[0])?;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].model_params_total_millions.is_none());
        assert!(rows[0].model_params_active_millions.is_none());
        Ok(())
    }

    /// Every nullable string column round-trips both a stored value (verbatim)
    /// and absence (as `None`, not an empty string) through Parquet. The
    /// descriptor case uses canonical form (keys sorted, no whitespace) as the
    /// ingest path stores it.
    #[rstest]
    #[case::model_name(
        "llama-3.2-1b",
        |r: &mut MetricRow, v: Option<String>| r.model_name = v,
        |r: &MetricRow| r.model_name.clone()
    )]
    #[case::model_quant(
        "q4_0",
        |r: &mut MetricRow, v: Option<String>| r.model_quant = v,
        |r: &MetricRow| r.model_quant.clone()
    )]
    #[case::model_descriptor(
        r#"{"filename":"q4_K_M.gguf","mmproj_filename":"mmproj-f16.gguf","org":"LiquidAI","repo_name":"LFM2.5-VL-450M-GGUF","type":"hf_gguf_vision"}"#,
        |r: &mut MetricRow, v: Option<String>| r.model_descriptor = v,
        |r: &MetricRow| r.model_descriptor.clone()
    )]
    #[case::runtime_name(
        "llama.cpp",
        |r: &mut MetricRow, v: Option<String>| r.runtime_name = v,
        |r: &MetricRow| r.runtime_name.clone()
    )]
    #[case::runtime_version(
        "b5000",
        |r: &mut MetricRow, v: Option<String>| r.runtime_version = v,
        |r: &MetricRow| r.runtime_version.clone()
    )]
    #[case::model_descriptor_sha256(
        "deadbeef",
        |r: &mut MetricRow, v: Option<String>| r.model_descriptor_sha256 = v,
        |r: &MetricRow| r.model_descriptor_sha256.clone()
    )]
    #[case::runtime_descriptor_sha256(
        "cafebabe",
        |r: &mut MetricRow, v: Option<String>| r.runtime_descriptor_sha256 = v,
        |r: &MetricRow| r.runtime_descriptor_sha256.clone()
    )]
    fn test_nullable_string_column_round_trip(
        #[case] value: &str,
        #[case] set: fn(&mut MetricRow, Option<String>),
        #[case] get: fn(&MetricRow) -> Option<String>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let read = |path: &std::path::Path| -> anyhow::Result<Vec<MetricRow>> {
            let file = std::fs::File::open(path)?;
            let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
            let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>()?;
            batch_to_rows(&batches[0])
        };

        // A value round-trips verbatim.
        let some_path = dir.path().join("some.parquet");
        let mut row = make_test_row("ttft", 100.0, "ms")?;
        set(&mut row, Some(value.to_string()));
        append_to_parquet(WriterOpts::default(), &some_path, &[row])?;
        let rows = read(&some_path)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(get(&rows[0]).as_deref(), Some(value));

        // Absence round-trips as `None`, not an empty string.
        let none_path = dir.path().join("none.parquet");
        let mut row = make_test_row("ttft", 100.0, "ms")?;
        set(&mut row, None);
        append_to_parquet(WriterOpts::default(), &none_path, &[row])?;
        let rows = read(&none_path)?;
        assert_eq!(rows.len(), 1);
        assert!(get(&rows[0]).is_none());
        Ok(())
    }

    #[test]
    fn test_rewrite_partition_preserves_score_runtime_version() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let partition = dir.path().join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let mut row1 = make_test_row("accuracy", 0.8, "ratio")?;
        row1.result_id = "job1_0".to_string();
        row1.score_runtime_version = Some("v1.0".to_string());
        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &[row1],
        )?;

        // Rewrite with a new job, existing job1 should be preserved
        let mut row2 = make_test_row("accuracy", 0.9, "ratio")?;
        row2.result_id = "job2_0".to_string();
        row2.score_runtime_version = Some("v1.1".to_string());
        write_partition(WriterOpts::default(), &partition, &[row2], 10_000)?;

        let rows = read_partition_rows(&partition)?;
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|r| r.result_id == "job1_0")
                .context("expected row with result_id job1_0")?
                .score_runtime_version
                .as_deref(),
            Some("v1.0")
        );
        assert_eq!(
            rows.iter()
                .find(|r| r.result_id == "job2_0")
                .context("expected row with result_id job2_0")?
                .score_runtime_version
                .as_deref(),
            Some("v1.1")
        );
        Ok(())
    }

    /// `client_version` survives the parquet round trip, and stays NULL —
    /// rather than becoming an empty string — for a row that never had one.
    #[test]
    fn test_client_version_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let partition = dir.path().join("month=2025-01");
        std::fs::create_dir_all(&partition)?;

        let mut reported = make_test_row("accuracy", 0.8, "ratio")?;
        reported.result_id = "job1_0".to_string();
        reported.client_version = Some("0.14.2".to_string());

        let mut silent = make_test_row("accuracy", 0.9, "ratio")?;
        silent.result_id = "job2_0".to_string();
        silent.client_version = None;

        write_partition_part(
            WriterOpts::default(),
            &partition,
            "part-0001.parquet",
            &[reported, silent],
        )?;

        let rows = read_partition_rows(&partition)?;
        let by_id = |id: &str| -> anyhow::Result<MetricRow> {
            rows.iter()
                .find(|r| r.result_id == id)
                .cloned()
                .with_context(|| format!("expected row with result_id {id}"))
        };
        assert_eq!(by_id("job1_0")?.client_version.as_deref(), Some("0.14.2"));
        assert_eq!(by_id("job2_0")?.client_version, None);
        Ok(())
    }

    /// Write a stand-in for an older build's output — one row of the current
    /// batch with `absent` projected away — then append `new_row` to that file
    /// and read every row back.
    ///
    /// The old row carries `result_id` `"job1_0"`, so give `new_row` a
    /// different one and tell the two apart with [`row_by_id`].
    fn append_to_file_without_columns(
        absent: &[&str],
        new_row: MetricRow,
    ) -> anyhow::Result<Vec<MetricRow>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.parquet");

        let mut old_row = make_test_row("accuracy", 0.8, "ratio")?;
        old_row.result_id = "job1_0".to_string();
        let schema = Arc::new(parquet_schema());
        let batch = rows_to_batch(&schema, &[old_row])?;
        let keep: Vec<usize> = (0..batch.num_columns())
            .filter(|&i| !absent.contains(&batch.schema().field(i).name().as_str()))
            .collect();
        assert_eq!(keep.len(), batch.num_columns() - absent.len());
        let old_batch = batch.project(&keep)?;

        let file = std::fs::File::create(&path)?;
        let mut writer = ArrowWriter::try_new(file, old_batch.schema(), None)?;
        writer.write(&old_batch)?;
        writer.close()?;

        append_to_parquet(WriterOpts::default(), &path, &[new_row])?;

        Ok(
            ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path)?)?
                .build()?
                .map(|b| batch_to_rows(&b?))
                .collect::<anyhow::Result<Vec<_>>>()?
                .concat(),
        )
    }

    /// The one row carrying `result_id`, or an error naming the id that is
    /// missing.
    fn row_by_id(rows: &[MetricRow], id: &str) -> anyhow::Result<MetricRow> {
        rows.iter()
            .find(|r| r.result_id == id)
            .cloned()
            .with_context(|| format!("expected row with result_id {id}"))
    }

    /// A file written before `client_version` existed still reads back — the
    /// column is absent from the file, not null within it — and appending a
    /// reporting row to it backfills the column without losing the old row.
    #[test]
    fn test_client_version_none_when_column_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.9, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.client_version = Some("0.14.2".to_string());

        let rows = append_to_file_without_columns(&["client_version"], new_row)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(row_by_id(&rows, "job1_0")?.client_version, None);
        assert_eq!(
            row_by_id(&rows, "job2_0")?.client_version.as_deref(),
            Some("0.14.2")
        );
        Ok(())
    }

    /// A file written before the memory-observation columns existed still reads
    /// back — the columns are absent from the file, not null within it — and
    /// appending a reporting row backfills them without losing the old row.
    #[test]
    fn test_observed_memory_none_when_columns_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.9, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.observation_max_swap_bytes = Some(6_442_450_944);
        new_row.observation_max_host_bytes = Some(12_884_901_888);

        let rows = append_to_file_without_columns(
            &["observation_max_swap_bytes", "observation_max_host_bytes"],
            new_row,
        )?;

        assert_eq!(rows.len(), 2);
        assert_eq!(row_by_id(&rows, "job1_0")?.observation_max_swap_bytes, None);
        assert_eq!(row_by_id(&rows, "job1_0")?.observation_max_host_bytes, None);
        assert_eq!(
            row_by_id(&rows, "job2_0")?.observation_max_swap_bytes,
            Some(6_442_450_944)
        );
        assert_eq!(
            row_by_id(&rows, "job2_0")?.observation_max_host_bytes,
            Some(12_884_901_888)
        );
        Ok(())
    }
}
