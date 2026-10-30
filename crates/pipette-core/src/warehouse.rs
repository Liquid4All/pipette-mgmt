use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, ListArray,
    StringArray, StructArray, TimestampMicrosecondArray, new_null_array,
};
use arrow::buffer::{NullBuffer, OffsetBuffer};
use arrow::datatypes::{DataType, Field, Fields, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use serde::Serialize;
use serde::de::{DeserializeOwned, IntoDeserializer};

use pipette_plan_types::{
    AndroidTemperatureSensor, AndroidThermalStatus, AppleThermalState, BatteryLevel, BenchmarkId,
    BenchmarkType, ClientId, DeviceFormFactor, DevicePowerState, FailureSource, JobId, JobMetric,
    LinuxThermalZone,
};

use crate::parquet_utils::{
    WriterOpts, read_batches_from_bytes, read_batches_from_file, write_batch_bytes,
    write_batches_to_file,
};

/// Shared by [`parquet_schema`] and the writer so their layouts cannot drift
/// (a `RecordBatch::try_new` type mismatch otherwise).
fn android_sensor_fields() -> Fields {
    Fields::from(vec![
        Field::new("iteration", DataType::Int32, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("celsius", DataType::Int32, false),
        Field::new("throttling_status", DataType::Utf8, false),
    ])
}

/// See [`android_sensor_fields`].
fn linux_zone_fields() -> Fields {
    Fields::from(vec![
        Field::new("iteration", DataType::Int32, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("celsius", DataType::Int32, false),
    ])
}

/// Arrow's default list child name is `item`; matching it keeps the schema
/// and the built array in lock-step.
fn list_item_field(fields: Fields) -> Field {
    Field::new("item", DataType::Struct(fields), true)
}

/// One `item` per measured repetition.
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
        // Nullable: a failure row carries no telemetry, and a `"system"`
        // failure ran on no device.
        Field::new("device_name", DataType::Utf8, true),
        Field::new("device_form_factor", DataType::Utf8, true),
        Field::new("device_os_name", DataType::Utf8, true),
        Field::new("device_os_version", DataType::Utf8, true),
        Field::new("device_os_build", DataType::Utf8, true),
        Field::new("device_os_security_patch", DataType::Utf8, true),
        Field::new("device_chip_model", DataType::Utf8, true),
        Field::new("device_gpu_model", DataType::Utf8, true),
        Field::new("device_gpu_vram_bytes", DataType::Int64, true),
        Field::new("device_npu_model", DataType::Utf8, true),
        Field::new("device_npu_vram_bytes", DataType::Int64, true),
        Field::new("device_ram_bytes", DataType::Int64, true),
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
        // Per-iteration thermal telemetry: `_before` at each rep's gate-pass,
        // `_after` once its timed work completes. A device fills only its own
        // platform's families; sensor/zone lists are tagged by `iteration`.
        // The worst condition over a run derives from these series, so it is not stored.
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
        // Raw iOS SoC die temperature (°C), one per repetition; null off iOS.
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
        // Nullable: a failure row records no measurement. `unit` stays
        // required, since the intended metric fixes it.
        Field::new("value", DataType::Float32, true),
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
        // Per-run metadata off the `value` axis, e.g. `{"samples_failed": N}`;
        // a JSON string so new keys need no schema change.
        Field::new("eval_metadata", DataType::Utf8, true),
        // From here on, columns are nullable and appended last (docs/storage.md).
        Field::new("parameter_num_images", DataType::Int32, true),
        // Observed VL throughput token counts (measured, not parameters).
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
        // Canonical JSON descriptors, each with a `_sha256` content id.
        Field::new("model_descriptor", DataType::Utf8, true),
        Field::new("runtime_descriptor", DataType::Utf8, true),
        Field::new("model_descriptor_sha256", DataType::Utf8, true),
        Field::new("runtime_descriptor_sha256", DataType::Utf8, true),
        // The resolved harness configuration, canonicalized and hashed alike.
        Field::new("benchmark_flags", DataType::Utf8, true),
        Field::new("benchmark_flags_sha256", DataType::Utf8, true),
        // Harness version, not the runtime's.
        Field::new("client_version", DataType::Utf8, true),
        // Content ids of the flags above, so "same config" is a join key.
        Field::new("model_flags_sha256", DataType::Utf8, true),
        Field::new("runtime_flags_sha256", DataType::Utf8, true),
        // Peak swap / host memory, denormalized onto every row.
        Field::new("observation_max_swap_bytes", DataType::Int64, true),
        Field::new("observation_max_host_bytes", DataType::Int64, true),
        // The flag groups the job asked for, from the claim's authored `spec`;
        // they differ from the effective ones by design.
        Field::new("requested_model_flags", DataType::Utf8, true),
        Field::new("requested_model_flags_sha256", DataType::Utf8, true),
        Field::new("requested_runtime_flags", DataType::Utf8, true),
        Field::new("requested_runtime_flags_sha256", DataType::Utf8, true),
        Field::new("requested_benchmark_flags", DataType::Utf8, true),
        Field::new("requested_benchmark_flags_sha256", DataType::Utf8, true),
        // A failure row sits on the measurement axis. Read `failed IS TRUE`: a
        // file written before the column reads NULL, not `false`.
        Field::new("failed", DataType::Boolean, true),
        Field::new("failed_reason", DataType::Utf8, true),
        Field::new("failure_source", DataType::Utf8, true),
        Field::new("failure_retriable", DataType::Boolean, true),
        // Derivable from `result_id`, but a per-cell tally counts distinct
        // `job_id`: a job has several rows, and a re-score appends copies.
        Field::new("job_id", DataType::Utf8, true),
        // Verbatim, with no `_sha256`: see `docs/storage.md`.
        Field::new("client_configuration", DataType::Utf8, true),
        // Corpus-benchmark parameters: the draft length, and the upstream
        // subset the corpus keeps (NULL = the whole upstream).
        Field::new("parameter_draft_length", DataType::Int32, true),
        Field::new("parameter_dataset_subset", DataType::Utf8, true),
        // Provenance for a corpus the server never holds; a mismatch is
        // recorded, not rejected.
        Field::new("observation_vl_spec_decode_turns", DataType::Int32, true),
        Field::new("observation_dataset_digest", DataType::Utf8, true),
        Field::new(
            "observation_dataset_digest_matched",
            DataType::Boolean,
            true,
        ),
        // `vl_spec_decode`: whether the drafter ran, and a reply digest that
        // pairs two rows for greedy identity.
        Field::new("runtime_speculative_enabled", DataType::Boolean, true),
        Field::new("observation_output_digest", DataType::Utf8, true),
        // Requests kept in flight at once; NULL on older rows, which all ran
        // at 1.
        Field::new("parameter_batch_size", DataType::Int32, true),
        // NULL where the client's default applied. `Float64` so
        // `WHERE parameter_temperature = 0.6` matches the literal.
        Field::new("parameter_temperature", DataType::Float64, true),
        Field::new("parameter_top_p", DataType::Float64, true),
        Field::new("parameter_top_k", DataType::Int32, true),
        Field::new("parameter_min_p", DataType::Float64, true),
        Field::new("parameter_seed", DataType::Int64, true),
        // Verbatim like `client_configuration`; see `docs/storage.md`.
        Field::new("client_extras", DataType::Utf8, true),
        // Device telemetry, read by nothing here.
        Field::new("device_hardware_readings", DataType::Utf8, true),
        Field::new("device_os_throttling_readings", DataType::Utf8, true),
        Field::new("readiness_gate_waits", DataType::Utf8, true),
        Field::new("model_vault_descriptor", DataType::Utf8, true),
        Field::new("rescored_from", DataType::Utf8, true),
        Field::new("rerun_of", DataType::Utf8, true),
        // The catalog's `BenchmarkParams` as canonical JSON, and its content id.
        Field::new("benchmark_params", DataType::Utf8, true),
        Field::new("benchmark_params_sha256", DataType::Utf8, true),
        Field::new("device_time_ms", DataType::Int64, true),
        // See `MetricRow::run_submitted_at`. Nullable and appended last.
        Field::new(
            "run_submitted_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        ),
        // The vlm_eval penalties, NULL where unpinned like the sampling
        // columns above.
        Field::new("parameter_presence_penalty", DataType::Float64, true),
        Field::new("parameter_repeat_penalty", DataType::Float64, true),
        // The vlm_eval's prompt profile, NULL where the entry names none and
        // every model got the dataset's own prompt.
        Field::new("parameter_prompt_profile", DataType::Utf8, true),
        // ASR: the clip's length and format (parameters), and the prefill
        // length and the audio-only portion the runtime actually produced
        // (measured, so observations like the VL token counts). NULL for every
        // other type, and on rows written before they existed. Appended after
        // the vlm_eval columns.
        Field::new("parameter_audio_duration_ms", DataType::Int32, true),
        Field::new("parameter_audio_sample_rate_hz", DataType::Int32, true),
        Field::new("parameter_audio_channels", DataType::Int32, true),
        Field::new(
            "observation_asr_throughput_prefill_tokens",
            DataType::Int32,
            true,
        ),
        Field::new(
            "observation_asr_throughput_audio_tokens",
            DataType::Int32,
            true,
        ),
        // ASR, as the client recorded it: how the context was sized (only an
        // `exact` peak or prefill is the clip's own) and, on throughput rows,
        // where the audio count came from. NULL for other types and earlier rows.
        Field::new("observation_asr_context_sizing", DataType::Utf8, true),
        Field::new("observation_asr_audio_tokens_source", DataType::Utf8, true),
        // ASR: whether the clip is longer than the model is documented to take
        // (the client's `asr_beyond_model_audio_limit`). NULL for other types
        // and earlier rows.
        Field::new(
            "observation_asr_beyond_model_audio_limit",
            DataType::Boolean,
            true,
        ),
        // The device's raw CPU architecture and OS family, as reported. NULL
        // on rows written before they existed.
        Field::new("device_cpu_arch", DataType::Utf8, true),
        Field::new("device_os_family", DataType::Utf8, true),
        // The asr_eval beam search, NULL where the entry decodes without it.
        Field::new("parameter_num_beams", DataType::Int32, true),
        Field::new("parameter_beam_repetition_penalty", DataType::Float64, true),
        Field::new("parameter_beam_length_penalty", DataType::Float64, true),
    ])
}

/// The reserved `client_id` a failure the server declares itself is stored
/// under. Registration derives ids from keys, so no machine holds it.
pub const SYSTEM_CLIENT_ID: &str = "system";

/// Whether `id` is [`SYSTEM_CLIENT_ID`], which names no machine.
pub fn is_system_client(id: &ClientId) -> bool {
    id.as_str() == SYSTEM_CLIENT_ID
}

#[derive(Clone)]
pub struct MetricRow {
    pub result_id: String,
    pub benchmark_id: BenchmarkId,
    pub benchmark_type: BenchmarkType,
    pub metric: String,
    pub client_id: ClientId,
    /// `None` on a failure row: it carries no telemetry, and a `"system"`
    /// failure ran on no device.
    pub device_name: Option<String>,
    pub device_form_factor: Option<DeviceFormFactor>,
    pub device_os_name: Option<String>,
    pub device_os_version: Option<String>,
    pub device_os_build: Option<String>,
    pub device_os_security_patch: Option<String>,
    pub device_cpu_arch: Option<String>,
    pub device_os_family: Option<String>,
    pub device_chip_model: Option<String>,
    pub device_gpu_model: Option<String>,
    pub device_gpu_vram_bytes: Option<i64>,
    pub device_npu_model: Option<String>,
    pub device_npu_vram_bytes: Option<i64>,
    /// Installed RAM, in bytes.
    pub device_ram_bytes: Option<i64>,
    pub device_battery_level: Option<BatteryLevel>,
    pub device_power_state: Option<DevicePowerState>,
    pub device_power_save_mode: Option<bool>,
    // Android only. `_cpuset` is the cgroup path, `_cpu_affinity_list` a Linux CPU
    // list, `_cpu_affinity_excludes_top_tier` whether the top-frequency tier is barred.
    pub device_android_cpuset: Option<String>,
    pub device_android_cpu_affinity_list: Option<String>,
    pub device_android_cpu_affinity_excludes_top_tier: Option<bool>,
    // Per-iteration thermal telemetry, `None` off the matching platform. The
    // sensor/zone families flatten (iteration, sensor) pairs into one list.
    pub device_apple_thermal_state_before: Option<Vec<AppleThermalState>>,
    pub device_apple_thermal_state_after: Option<Vec<AppleThermalState>>,
    /// Raw iOS SoC die temperature (°C), one per repetition; `None` off iOS.
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
    // Device telemetry: a JSON array, or `None`, as on every failure row; see
    // `docs/storage.md`.
    pub device_hardware_readings: Option<String>,
    pub device_os_throttling_readings: Option<String>,
    pub readiness_gate_waits: Option<String>,
    pub model_name: Option<String>,
    pub model_quant: Option<String>,
    pub model_params_total_millions: Option<i32>,
    pub model_params_active_millions: Option<i32>,
    /// Canonical JSON when the client sent JSON, else the trimmed string;
    /// `None` when absent or `{}`.
    pub model_flags: Option<String>,
    /// Display name only; the identity lives in `runtime_descriptor`.
    pub runtime_name: Option<String>,
    /// Display version only; the identity lives in `runtime_descriptor`.
    pub runtime_version: Option<String>,
    /// Opaque configuration affecting the runtime itself, normalized exactly
    /// like [`Self::model_flags`].
    pub runtime_flags: Option<String>,
    pub runtime_cpu_variant: Option<String>,
    /// `None` on a failure row, which records no value for its intended metric.
    pub value: Option<f32>,
    pub value_stddev: Option<f32>,
    /// Set on failure rows too: the intended metric fixes the unit.
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
    /// Measured VL token counts (total prefill and image-only). Model-dependent,
    /// so observations rather than parameters.
    pub observation_vl_throughput_prefill_tokens: Option<i32>,
    pub observation_vl_throughput_image_tokens: Option<i32>,
    pub score_runtime_version: Option<String>,
    /// JSON object of per-run metadata, e.g. `{"samples_failed": N}` from evals.
    pub eval_metadata: Option<String>,
    /// Canonical JSON, opaque to the server: stored and pattern-searched, never
    /// deserialized.
    pub model_descriptor: Option<String>,
    /// Canonical JSON, like [`Self::model_descriptor`], with version and build baked in.
    pub runtime_descriptor: Option<String>,
    /// Hex `sha256` of the canonical `model_descriptor`, computed server-side.
    pub model_descriptor_sha256: Option<String>,
    /// The descriptor the device reported, set when the model ran from the vault
    /// as a copy of a public one ([`Self::model_descriptor`] then names that).
    pub model_vault_descriptor: Option<String>,
    /// Hex `sha256` of the canonical `runtime_descriptor`, computed server-side.
    pub runtime_descriptor_sha256: Option<String>,
    /// The resolved harness configuration (readiness gating, timeouts, loop
    /// detection) as canonical JSON.
    pub benchmark_flags: Option<String>,
    /// Hex `sha256` of the canonical `benchmark_flags`, computed server-side;
    /// the grouping key for "runs measured the same way".
    pub benchmark_flags_sha256: Option<String>,
    /// The harness build, not [`Self::runtime_version`]: one runtime measured by
    /// two harness versions can differ.
    pub client_version: Option<String>,
    /// Hex `sha256` of the canonical `model_flags`, computed server-side.
    pub model_flags_sha256: Option<String>,
    /// Hex `sha256` of the canonical `runtime_flags`, computed server-side; the
    /// grouping key for "runs configured the same way".
    pub runtime_flags_sha256: Option<String>,
    /// Per-run memory observation in bytes, on every row; see
    /// [`pipette_plan_types::MemoryObservation`]. Not the `max_host_usage`
    /// metric, which is the `value` of its own row.
    pub observation_max_swap_bytes: Option<i64>,
    pub observation_max_host_bytes: Option<i64>,
    /// The `model_flags` group the job asked for, read from the claim's authored
    /// `spec` (so no client supplies it), `auth_token`s dropped. `None` means no
    /// request was recorded, never an empty one. Differs from
    /// [`Self::model_flags`] by design.
    pub requested_model_flags: Option<String>,
    /// Hex `sha256` of [`Self::requested_model_flags`].
    pub requested_model_flags_sha256: Option<String>,
    /// The `runtime_flags` group the job asked for; see
    /// [`Self::requested_model_flags`].
    pub requested_runtime_flags: Option<String>,
    /// Hex `sha256` of [`Self::requested_runtime_flags`].
    pub requested_runtime_flags_sha256: Option<String>,
    /// The `benchmark_flags` group the job asked for; see
    /// [`Self::requested_model_flags`].
    pub requested_benchmark_flags: Option<String>,
    /// Hex `sha256` of [`Self::requested_benchmark_flags`].
    pub requested_benchmark_flags_sha256: Option<String>,
    /// `None` only in files written before the column; query `failed IS TRUE`.
    pub failed: Option<bool>,
    /// A client's `failure_reason`, or the server's message when it retired the job.
    pub failed_reason: Option<String>,
    /// Which pass produced the failure. `None` on a success row, and on a
    /// failure body stored before the field existed.
    pub failure_source: Option<FailureSource>,
    /// `Some(false)` on every `"system"` failure, which is terminal.
    pub failure_retriable: Option<bool>,
    /// `None` only on rows from files written before the column. Count distinct
    /// `job_id` for a per-cell tally.
    pub job_id: Option<String>,
    /// The job whose record this row's job copies to score again (a
    /// recalculation or `requeue-eval`). `None` on every row a device ran.
    pub rescored_from: Option<String>,
    /// The job this row's job runs again, for a job a campaign re-run minted.
    /// `None` on every other row.
    pub rerun_of: Option<String>,
    /// When a device submitted the run that this row scores, in microseconds
    /// since the epoch. Equal to `submitted_at` on every row that is not a copy.
    /// On a copy (see [`Self::rescored_from`]) it is the time of the device run
    /// that the copy scores again. `None` on a copy whose chain of copies
    /// reaches a record that is gone or is not a success, on a copy staged
    /// before copies carried the time, and on a row read back from a file
    /// written before the column existed. See `docs/storage.md`.
    pub run_submitted_at: Option<i64>,
    /// Stored exactly as sent; `None` when the submission carried none. See
    /// `docs/storage.md`.
    pub client_configuration: Option<String>,
    /// Stored exactly as sent; `None` when the submission carried none. See
    /// `docs/storage.md`.
    pub client_extras: Option<String>,
    /// ASR: the clip as sent (length, rate, channels). NULL for other types.
    pub parameter_audio_duration_ms: Option<i32>,
    pub parameter_audio_sample_rate_hz: Option<i32>,
    pub parameter_audio_channels: Option<i32>,
    /// `asr_throughput`, measured: the total prefill length and the audio-only
    /// portion of it. Model-dependent outputs (each encoder makes its own count
    /// of the same clip), so observations beside every metric row and not
    /// metrics. NULL for other types.
    pub observation_asr_throughput_prefill_tokens: Option<i32>,
    pub observation_asr_throughput_audio_tokens: Option<i32>,
    /// `asr_throughput` / `asr_max_memory`: `exact`, `upper_bound` or `pinned`
    /// (`AsrContextSizing`). Only an `exact` row is the clip's own peak or
    /// prefill. NULL for other types.
    pub observation_asr_context_sizing: Option<String>,
    /// `asr_throughput`: `calibrated` or `measured` (`AsrAudioTokensSource`).
    pub observation_asr_audio_tokens_source: Option<String>,
    /// `asr_throughput` / `asr_max_memory`: `true` when the clip is longer than
    /// the model is documented to take, so the row is a speed or memory
    /// scenario whose transcription quality is not established. Required at
    /// intake on both types; NULL for other types and failure rows.
    pub observation_asr_beyond_model_audio_limit: Option<bool>,
    /// `vl_spec_decode`: drafted tokens per verification step, and the corpus's
    /// upstream subset (`None` = the whole upstream).
    pub parameter_draft_length: Option<i32>,
    pub parameter_dataset_subset: Option<String>,
    /// Requests in flight at once, on every type that takes a batch (see
    /// `Benchmark::parameter_batch_size`); `None` elsewhere.
    pub parameter_batch_size: Option<i32>,
    /// `None` is "unpinned", never a value: the client's own default applied,
    /// or on a vlm_eval greedy with every other sampler neutral.
    pub parameter_temperature: Option<f64>,
    pub parameter_top_p: Option<f64>,
    pub parameter_top_k: Option<i32>,
    pub parameter_min_p: Option<f64>,
    pub parameter_presence_penalty: Option<f64>,
    pub parameter_repeat_penalty: Option<f64>,
    pub parameter_seed: Option<i64>,
    /// The vlm_eval's prompt profile; `None` is the dataset's own prompt.
    pub parameter_prompt_profile: Option<String>,
    /// The asr_eval's beam width and its two penalties; `None` decodes without
    /// beams (and on every other type).
    pub parameter_num_beams: Option<i32>,
    pub parameter_beam_repetition_penalty: Option<f64>,
    pub parameter_beam_length_penalty: Option<f64>,
    /// The catalog entry's `BenchmarkParams` as canonical JSON, tagged with its
    /// `benchmark_type`: every `parameter_*` knob but the corpus subset.
    pub benchmark_params: Option<String>,
    /// Hex `sha256` of [`Self::benchmark_params`].
    pub benchmark_params_sha256: Option<String>,
    /// Milliseconds the device spent from the claim until the result or failure
    /// was ready to submit, as the client reported it.
    pub device_time_ms: Option<i64>,
    /// Turns the run actually traversed — a measured fact, because a corpus we
    /// do not store can be a different size than the catalog believed.
    pub observation_vl_spec_decode_turns: Option<i32>,
    /// The corpus the client verified, and whether that equalled the digest the
    /// catalog pins.
    pub observation_dataset_digest: Option<String>,
    pub observation_dataset_digest_matched: Option<bool>,
    /// `vl_spec_decode`: whether the drafter ran, as the client resolved
    /// `runtime_flags.speculative_enabled`. `None` on every other type.
    pub runtime_speculative_enabled: Option<bool>,
    /// sha256 over the traversal's replies in order: equal digests on two rows
    /// over the same corpus mean greedy-identical output.
    pub observation_output_digest: Option<String>,
}

#[cfg(any(test, feature = "testkit"))]
impl Default for MetricRow {
    /// Tests override fields via `..Default::default()`. Behind `testkit`: the
    /// validated id newtypes have no meaningful empty default.
    fn default() -> Self {
        Self {
            result_id: "r1".into(),
            job_id: Some("r1".into()),
            benchmark_id: crate::types::id_from_valid(
                "test".to_string(),
                "the fixture's benchmark id satisfies the charset",
            ),
            benchmark_type: BenchmarkType::PrefillThroughput,
            metric: String::new(),
            client_id: crate::types::id_from_valid(
                "ev1_c".to_string(),
                "the fixture's client id satisfies the charset",
            ),
            device_name: Some("test-device".into()),
            device_form_factor: Some(DeviceFormFactor::Embedded),
            device_os_name: Some("Linux".into()),
            device_os_version: Some("22.04".into()),
            device_os_build: None,
            device_os_security_patch: None,
            device_cpu_arch: None,
            device_os_family: None,
            device_chip_model: Some("test-chip".into()),
            device_gpu_model: None,
            device_gpu_vram_bytes: None,
            device_npu_model: None,
            device_npu_vram_bytes: None,
            device_ram_bytes: Some(17_179_869_184),
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
            device_hardware_readings: None,
            device_os_throttling_readings: None,
            readiness_gate_waits: None,
            model_name: Some("model".into()),
            model_quant: Some("q4".into()),
            model_params_total_millions: None,
            model_params_active_millions: None,
            model_flags: None,
            runtime_name: Some("rt".into()),
            runtime_version: Some("v1".into()),
            runtime_flags: None,
            runtime_cpu_variant: None,
            value: Some(0.0),
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
            model_vault_descriptor: None,
            rescored_from: None,
            rerun_of: None,
            run_submitted_at: None,
            runtime_descriptor_sha256: None,
            benchmark_flags: None,
            benchmark_flags_sha256: None,
            model_flags_sha256: None,
            runtime_flags_sha256: None,
            client_version: None,
            observation_max_swap_bytes: None,
            observation_max_host_bytes: None,
            requested_model_flags: None,
            requested_model_flags_sha256: None,
            requested_runtime_flags: None,
            requested_runtime_flags_sha256: None,
            requested_benchmark_flags: None,
            requested_benchmark_flags_sha256: None,
            failed: Some(false),
            failed_reason: None,
            failure_source: None,
            failure_retriable: None,
            client_configuration: None,
            client_extras: None,
            parameter_audio_duration_ms: None,
            parameter_audio_sample_rate_hz: None,
            parameter_audio_channels: None,
            observation_asr_throughput_prefill_tokens: None,
            observation_asr_throughput_audio_tokens: None,
            observation_asr_context_sizing: None,
            observation_asr_audio_tokens_source: None,
            observation_asr_beyond_model_audio_limit: None,
            parameter_draft_length: None,
            parameter_dataset_subset: None,
            parameter_batch_size: None,
            parameter_temperature: None,
            parameter_top_p: None,
            parameter_top_k: None,
            parameter_min_p: None,
            parameter_presence_penalty: None,
            parameter_repeat_penalty: None,
            parameter_seed: None,
            parameter_prompt_profile: None,
            parameter_num_beams: None,
            parameter_beam_repetition_penalty: None,
            parameter_beam_length_penalty: None,
            benchmark_params: None,
            benchmark_params_sha256: None,
            device_time_ms: None,
            observation_vl_spec_decode_turns: None,
            observation_dataset_digest: None,
            observation_dataset_digest_matched: None,
            runtime_speculative_enabled: None,
            observation_output_digest: None,
        }
    }
}

/// Derive a `YYYY-MM-DD` day key from a microsecond-precision UTC timestamp.
pub fn day_key_from_timestamp(timestamp_us: i64) -> anyhow::Result<String> {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = DateTime::from_timestamp_micros(timestamp_us)
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp: {timestamp_us}"))?;
    Ok(dt.format("%Y-%m-%d").to_string())
}

/// Whether the day partition holding `submitted_at` still accepts writes. The
/// lock window runs from midnight UTC of the partition's day, so "open" holds
/// for every row in it (`Config::warehouse_lock_age_secs` says why).
pub fn day_partition_is_open(
    submitted_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    lock_age: chrono::TimeDelta,
) -> bool {
    let day_start = submitted_at
        .date_naive()
        .and_time(chrono::NaiveTime::MIN)
        .and_utc();
    now.signed_duration_since(day_start) < lock_age
}

/// New writes use `day=` partitions; legacy `month=` ones are only read (see
/// [`read_metrics_for_job`]).
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

/// Legacy `month=` partition dir, kept for reads and legacy fixtures only.
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

/// A partition key's date: a `day=` key's own, a legacy `month=` key's **last
/// day** (so the month overlaps any window one of its days would).
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

/// Partitions overlapping `[today - read_days, today]`, all `day=` newest first,
/// then legacy `month=`. Days first finds recent jobs fast, and a job written to
/// both schemes around the cutover resolves to its newer `day=` copy.
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

/// Pad a batch to `target_schema` with all-null columns for missing nullable
/// fields. Errors on a missing non-nullable or an extra column, rather than
/// silently dropping data.
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
/// Append-only, no dedup: only the tail part is rewritten, so re-scoring a
/// `job_id` appends a second copy and readers must tolerate it.
pub fn write_partition(
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

fn read_part_rows(path: &Path) -> anyhow::Result<Vec<MetricRow>> {
    Ok(read_batches_from_file(path)?
        .map(|b| batch_to_rows(&b?))
        .collect::<anyhow::Result<Vec<_>>>()?
        .concat())
}

pub fn part_index(file_name: &str) -> Option<usize> {
    file_name
        .strip_prefix("part-")
        .and_then(|s| s.strip_suffix(".parquet"))
        .and_then(|s| s.parse::<usize>().ok())
}

/// The name a parquet body is staged under before its rename into `parent`.
///
/// Never `.parquet`: readers select on that extension, so a staged file a
/// crash strands would read back as data.
pub fn parquet_staging_path(parent: &Path) -> PathBuf {
    parent.join(format!(".tmp-{}.tmp", uuid::Uuid::new_v4()))
}

/// Write one part file via tmp+rename so replacing the tail part is atomic.
fn write_parquet_atomic(opts: WriterOpts, path: &Path, rows: &[MetricRow]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("part path has no parent: {}", path.display()))?;
    let tmp = parquet_staging_path(parent);
    let guard = crate::tmp_file::TmpFileGuard::new(tmp.clone());
    write_parquet(opts, &tmp, rows)?;
    std::fs::rename(&tmp, path)?;
    guard.disarm();
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

pub fn rows_to_parquet_bytes(opts: WriterOpts, rows: &[MetricRow]) -> anyhow::Result<Vec<u8>> {
    let schema = Arc::new(parquet_schema());
    let batch = rows_to_batch(&schema, rows)?;
    write_batch_bytes(opts, schema, &batch)
}

pub fn rows_from_parquet_bytes(data: &[u8]) -> anyhow::Result<Vec<MetricRow>> {
    let mut rows = Vec::new();
    for batch in read_batches_from_bytes(data)? {
        rows.extend(batch_to_rows(&batch?)?);
    }
    Ok(rows)
}

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

fn struct_child<'a, A: arrow::array::Array + 'static>(
    structs: &'a StructArray,
    child: &str,
    col: &str,
) -> anyhow::Result<&'a A> {
    structs
        .column_by_name(child)
        .ok_or_else(|| anyhow::anyhow!("{col} column struct missing '{child}' field"))?
        .as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| anyhow::anyhow!("{col} column '{child}' field has unexpected type"))
}

/// A column absent from an old file reads all-`None`; a null slot, `None`.
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
            let iterations = struct_child::<Int32Array>(structs, "iteration", name)?;
            let types = struct_child::<StringArray>(structs, "type", name)?;
            let names = struct_child::<StringArray>(structs, "name", name)?;
            let celsius = struct_child::<Int32Array>(structs, "celsius", name)?;
            let statuses = struct_child::<StringArray>(structs, "throttling_status", name)?;
            let elems = (0..structs.len())
                .map(|j| {
                    Ok(AndroidTemperatureSensor {
                        iteration: iterations.value(j),
                        sensor_type: types.value(j).to_string(),
                        name: names.value(j).to_string(),
                        celsius: celsius.value(j),
                        throttling_status: enum_from_column(statuses.value(j))
                            .map_err(|e| anyhow::anyhow!("{name}: {e}"))?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Some(elems))
        })
        .collect()
}

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
            let iterations = struct_child::<Int32Array>(structs, "iteration", name)?;
            let types = struct_child::<StringArray>(structs, "type", name)?;
            let celsius = struct_child::<Int32Array>(structs, "celsius", name)?;
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

/// An enum column holds the value's serde spelling, the one the submission
/// arrived in, so the warehouse and the wire cannot spell a variant two ways.
fn enum_to_column<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let serde_json::Value::String(s) = serde_json::to_value(value)? else {
        anyhow::bail!("an enum column holds unit variants only");
    };
    Ok(s)
}

fn enum_from_column<T: DeserializeOwned>(s: &str) -> Result<T, serde::de::value::Error> {
    T::deserialize(s.into_deserializer())
}

/// Absent column → all `None`; null list slot → row `None`.
fn read_enum_list_column<T>(batch: &RecordBatch, name: &str) -> anyhow::Result<Vec<Option<Vec<T>>>>
where
    T: DeserializeOwned + Clone,
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
                    enum_from_column(strs.value(j)).map_err(|e| anyhow::anyhow!("{name}: {e}"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Some(elems))
        })
        .collect()
}

/// Row `i` of a per-row list column; the readers emit one entry per batch row.
fn list_cell<T: Clone>(col: &[Option<Vec<T>>], i: usize) -> anyhow::Result<Option<Vec<T>>> {
    col.get(i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("row {i}: list column shorter than the batch"))
}

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

fn build_scalar_list_array(
    item: DataType,
    values: ArrayRef,
    lists_len: &[Option<usize>],
) -> ArrayRef {
    let validity: Vec<bool> = lists_len.iter().map(|l| l.is_some()).collect();
    let offsets = OffsetBuffer::from_lengths(lists_len.iter().map(|len| len.unwrap_or(0)));
    Arc::new(ListArray::new(
        Arc::new(Field::new("item", item, true)),
        offsets,
        values,
        Some(NullBuffer::from(validity)),
    ))
}

fn build_enum_list_array<'a, T: Serialize + 'a>(
    rows: &'a [MetricRow],
    field: impl Fn(&'a MetricRow) -> Option<&'a [T]>,
) -> anyhow::Result<ArrayRef> {
    let lens: Vec<Option<usize>> = rows.iter().map(|r| field(r).map(<[T]>::len)).collect();
    let values = rows
        .iter()
        .filter_map(&field)
        .flatten()
        .map(enum_to_column)
        .collect::<anyhow::Result<Vec<String>>>()?;
    Ok(build_scalar_list_array(
        DataType::Utf8,
        Arc::new(StringArray::from(values)),
        &lens,
    ))
}

fn build_f32_list_array(lists: &[Option<Vec<f32>>]) -> ArrayRef {
    let lens: Vec<Option<usize>> = lists.iter().map(|l| l.as_ref().map(Vec::len)).collect();
    let values: Vec<f32> = lists.iter().flatten().flatten().copied().collect();
    build_scalar_list_array(
        DataType::Float32,
        Arc::new(Float32Array::from(values)),
        &lens,
    )
}

fn build_android_sensors_array(
    lists: &[Option<Vec<AndroidTemperatureSensor>>],
) -> anyhow::Result<ArrayRef> {
    let validity: Vec<bool> = lists.iter().map(|l| l.is_some()).collect();
    let offsets =
        OffsetBuffer::from_lengths(lists.iter().map(|list| list.as_ref().map_or(0, Vec::len)));
    let sensors = || lists.iter().flatten().flatten();
    let iterations: Vec<i32> = sensors().map(|e| e.iteration).collect();
    let types: Vec<&str> = sensors().map(|e| e.sensor_type.as_str()).collect();
    let names: Vec<&str> = sensors().map(|e| e.name.as_str()).collect();
    let celsius: Vec<i32> = sensors().map(|e| e.celsius).collect();
    let statuses = sensors()
        .map(|e| enum_to_column(&e.throttling_status))
        .collect::<anyhow::Result<Vec<String>>>()?;
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
    Ok(Arc::new(ListArray::new(
        Arc::new(list_item_field(android_sensor_fields())),
        offsets,
        Arc::new(structs),
        Some(NullBuffer::from(validity)),
    )))
}

fn build_linux_zones_array(lists: &[Option<Vec<LinuxThermalZone>>]) -> ArrayRef {
    let validity: Vec<bool> = lists.iter().map(|l| l.is_some()).collect();
    let offsets =
        OffsetBuffer::from_lengths(lists.iter().map(|list| list.as_ref().map_or(0, Vec::len)));
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
        offsets,
        Arc::new(structs),
        Some(NullBuffer::from(validity)),
    ))
}

/// `None` when the file carries no such column, so a file written before the
/// column existed reads it null instead of failing.
fn optional_column<'a, A: arrow::array::Array + 'static>(
    batch: &'a RecordBatch,
    name: &str,
) -> anyhow::Result<Option<&'a A>> {
    batch
        .column_by_name(name)
        .map(|column| {
            column
                .as_any()
                .downcast_ref::<A>()
                .ok_or_else(|| anyhow::anyhow!("{name} column has unexpected type"))
        })
        .transpose()
}

fn optional_string_column<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> anyhow::Result<Option<&'a StringArray>> {
    optional_column(batch, name)
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
    // Lenient: a column absent from older parquet reads back all-null.
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
    let device_cpu_arch_col = optional_string_column(batch, "device_cpu_arch")?;
    let device_os_family_col = optional_string_column(batch, "device_os_family")?;
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
    // Read as None when absent so `fix-canonical` can backfill them.
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
    let runtime_cpu_variant_col = optional_string_column(batch, "runtime_cpu_variant")?;
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
    let num_images = batch
        .column_by_name("parameter_num_images")
        .map(|c| {
            c.as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| anyhow::anyhow!("parameter_num_images column has unexpected type"))
        })
        .transpose()?;
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
    let score_runtime_versions = optional_string_column(batch, "score_runtime_version")?;
    let eval_metadatas = optional_string_column(batch, "eval_metadata")?;
    let obs_swap_bytes = batch
        .column_by_name("observation_max_swap_bytes")
        .map(|c| {
            c.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
                anyhow::anyhow!("observation_max_swap_bytes column has unexpected type")
            })
        })
        .transpose()?;
    let obs_host_bytes = batch
        .column_by_name("observation_max_host_bytes")
        .map(|c| {
            c.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
                anyhow::anyhow!("observation_max_host_bytes column has unexpected type")
            })
        })
        .transpose()?;

    let requested_model_flags = optional_string_column(batch, "requested_model_flags")?;
    let requested_model_flags_sha256 =
        optional_string_column(batch, "requested_model_flags_sha256")?;
    let requested_runtime_flags = optional_string_column(batch, "requested_runtime_flags")?;
    let requested_runtime_flags_sha256 =
        optional_string_column(batch, "requested_runtime_flags_sha256")?;
    let requested_benchmark_flags = optional_string_column(batch, "requested_benchmark_flags")?;
    let requested_benchmark_flags_sha256 =
        optional_string_column(batch, "requested_benchmark_flags_sha256")?;

    let failed_col = batch
        .column_by_name("failed")
        .map(|c| {
            c.as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(|| anyhow::anyhow!("failed column has unexpected type"))
        })
        .transpose()?;
    let failed_reason_col = optional_string_column(batch, "failed_reason")?;
    let failure_source_col = optional_string_column(batch, "failure_source")?;
    let job_id_col = optional_string_column(batch, "job_id")?;
    let client_configuration_col = optional_string_column(batch, "client_configuration")?;
    let client_extras_col = optional_string_column(batch, "client_extras")?;
    let hardware_readings_col = optional_string_column(batch, "device_hardware_readings")?;
    let os_throttling_readings_col =
        optional_string_column(batch, "device_os_throttling_readings")?;
    let readiness_gate_waits_col = optional_string_column(batch, "readiness_gate_waits")?;
    let model_vault_descriptor_col = optional_string_column(batch, "model_vault_descriptor")?;
    let rescored_from_col = optional_string_column(batch, "rescored_from")?;
    let rerun_of_col = optional_string_column(batch, "rerun_of")?;
    let benchmark_params_col = optional_string_column(batch, "benchmark_params")?;
    let benchmark_params_sha256_col = optional_string_column(batch, "benchmark_params_sha256")?;
    let device_time_ms_col = optional_column::<Int64Array>(batch, "device_time_ms")?;
    let run_submitted_at_col =
        optional_column::<TimestampMicrosecondArray>(batch, "run_submitted_at")?;
    let audio_duration_col = optional_column::<Int32Array>(batch, "parameter_audio_duration_ms")?;
    let audio_sample_rate_col =
        optional_column::<Int32Array>(batch, "parameter_audio_sample_rate_hz")?;
    let audio_channels_col = optional_column::<Int32Array>(batch, "parameter_audio_channels")?;
    let asr_prompt_tokens_col =
        optional_column::<Int32Array>(batch, "observation_asr_throughput_prefill_tokens")?;
    let asr_audio_tokens_col =
        optional_column::<Int32Array>(batch, "observation_asr_throughput_audio_tokens")?;
    let asr_context_sizing_col = optional_string_column(batch, "observation_asr_context_sizing")?;
    let asr_audio_tokens_source_col =
        optional_string_column(batch, "observation_asr_audio_tokens_source")?;
    let asr_beyond_limit_col =
        optional_column::<BooleanArray>(batch, "observation_asr_beyond_model_audio_limit")?;
    let draft_length_col = optional_column::<Int32Array>(batch, "parameter_draft_length")?;
    let dataset_subset_col = optional_string_column(batch, "parameter_dataset_subset")?;
    let batch_size_col = optional_column::<Int32Array>(batch, "parameter_batch_size")?;
    let temperature_col = optional_column::<Float64Array>(batch, "parameter_temperature")?;
    let top_p_col = optional_column::<Float64Array>(batch, "parameter_top_p")?;
    let top_k_col = optional_column::<Int32Array>(batch, "parameter_top_k")?;
    let min_p_col = optional_column::<Float64Array>(batch, "parameter_min_p")?;
    let seed_col = optional_column::<Int64Array>(batch, "parameter_seed")?;
    let presence_penalty_col =
        optional_column::<Float64Array>(batch, "parameter_presence_penalty")?;
    let repeat_penalty_col = optional_column::<Float64Array>(batch, "parameter_repeat_penalty")?;
    let prompt_profile_col = optional_string_column(batch, "parameter_prompt_profile")?;
    let num_beams_col = optional_column::<Int32Array>(batch, "parameter_num_beams")?;
    let beam_repetition_penalty_col =
        optional_column::<Float64Array>(batch, "parameter_beam_repetition_penalty")?;
    let beam_length_penalty_col =
        optional_column::<Float64Array>(batch, "parameter_beam_length_penalty")?;
    let vl_spec_decode_turns_col =
        optional_column::<Int32Array>(batch, "observation_vl_spec_decode_turns")?;
    let dataset_digest_col = optional_string_column(batch, "observation_dataset_digest")?;
    let dataset_digest_matched_col =
        optional_column::<BooleanArray>(batch, "observation_dataset_digest_matched")?;
    let speculative_enabled_col =
        optional_column::<BooleanArray>(batch, "runtime_speculative_enabled")?;
    let output_digest_col = optional_string_column(batch, "observation_output_digest")?;
    let failure_retriable_col = batch
        .column_by_name("failure_retriable")
        .map(|c| {
            c.as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(|| anyhow::anyhow!("failure_retriable column has unexpected type"))
        })
        .transpose()?;

    let mut rows = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        rows.push(MetricRow {
            result_id: result_ids.value(i).to_string(),
            benchmark_id: BenchmarkId::try_new(benchmark_ids.value(i))?,
            benchmark_type: enum_from_column(benchmark_types.value(i))
                .map_err(|e| anyhow::anyhow!("row {i}: {e}"))?,
            metric: metrics.value(i).to_string(),
            client_id: ClientId::try_new(client_ids.value(i))?,
            device_name: (!device_names.is_null(i)).then(|| device_names.value(i).to_string()),
            device_form_factor: (!device_form_factors.is_null(i))
                .then(|| {
                    enum_from_column(device_form_factors.value(i))
                        .map_err(|e| anyhow::anyhow!("row {i}: {e}"))
                })
                .transpose()?,
            device_os_name: (!device_os_names.is_null(i))
                .then(|| device_os_names.value(i).to_string()),
            device_os_version: (!device_os_versions.is_null(i))
                .then(|| device_os_versions.value(i).to_string()),
            device_os_build: device_os_build_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_os_security_patch: device_os_security_patch_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_cpu_arch: device_cpu_arch_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_os_family: device_os_family_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_chip_model: (!device_chip_models.is_null(i))
                .then(|| device_chip_models.value(i).to_string()),
            device_gpu_model: (!device_gpu_models.is_null(i))
                .then(|| device_gpu_models.value(i).to_string()),
            device_gpu_vram_bytes: (!device_gpu_vram_bytes.is_null(i))
                .then(|| device_gpu_vram_bytes.value(i)),
            device_npu_model: (!device_npu_models.is_null(i))
                .then(|| device_npu_models.value(i).to_string()),
            device_npu_vram_bytes: (!device_npu_vram_bytes.is_null(i))
                .then(|| device_npu_vram_bytes.value(i)),
            device_ram_bytes: (!device_ram_bytes.is_null(i)).then(|| device_ram_bytes.value(i)),
            device_battery_level: device_battery_level_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i)))
                .map(BatteryLevel::try_new)
                .transpose()
                .map_err(|e| anyhow::anyhow!("row {i}: device_battery_level: {e}"))?,
            device_power_state: device_power_state_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i)))
                .map(|s| {
                    enum_from_column(s)
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
            device_apple_thermal_state_before: list_cell(
                &device_apple_thermal_state_before_col,
                i,
            )?,
            device_apple_thermal_state_after: list_cell(&device_apple_thermal_state_after_col, i)?,
            device_apple_soc_temp_c_before: list_cell(&device_apple_soc_temp_c_before_col, i)?,
            device_apple_soc_temp_c_after: list_cell(&device_apple_soc_temp_c_after_col, i)?,
            device_android_thermal_status_before: list_cell(
                &device_android_thermal_status_before_col,
                i,
            )?,
            device_android_thermal_status_after: list_cell(
                &device_android_thermal_status_after_col,
                i,
            )?,
            device_android_thermal_headroom_before: list_cell(
                &device_android_thermal_headroom_before_col,
                i,
            )?,
            device_android_thermal_headroom_after: list_cell(
                &device_android_thermal_headroom_after_col,
                i,
            )?,
            device_android_thermal_sensors_before: list_cell(
                &device_android_thermal_sensors_before_col,
                i,
            )?,
            device_android_thermal_sensors_after: list_cell(
                &device_android_thermal_sensors_after_col,
                i,
            )?,
            device_linux_thermal_zones_before: list_cell(
                &device_linux_thermal_zones_before_col,
                i,
            )?,
            device_linux_thermal_zones_after: list_cell(&device_linux_thermal_zones_after_col, i)?,
            device_hardware_readings: hardware_readings_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_os_throttling_readings: os_throttling_readings_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            readiness_gate_waits: readiness_gate_waits_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            model_vault_descriptor: model_vault_descriptor_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            rescored_from: rescored_from_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            rerun_of: rerun_of_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            run_submitted_at: run_submitted_at_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
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
            value: (!values.is_null(i)).then(|| values.value(i)),
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
            requested_model_flags: requested_model_flags
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            requested_model_flags_sha256: requested_model_flags_sha256
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            requested_runtime_flags: requested_runtime_flags
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            requested_runtime_flags_sha256: requested_runtime_flags_sha256
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            requested_benchmark_flags: requested_benchmark_flags
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            requested_benchmark_flags_sha256: requested_benchmark_flags_sha256
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            failed: failed_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            failed_reason: failed_reason_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            failure_source: failure_source_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i)))
                .map(|s| {
                    s.parse::<FailureSource>()
                        .map_err(|e| anyhow::anyhow!("row {i}: failure_source: {e}"))
                })
                .transpose()?,
            failure_retriable: failure_retriable_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            job_id: job_id_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            client_configuration: client_configuration_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            client_extras: client_extras_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            parameter_audio_duration_ms: audio_duration_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_audio_sample_rate_hz: audio_sample_rate_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_audio_channels: audio_channels_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_asr_throughput_prefill_tokens: asr_prompt_tokens_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_asr_throughput_audio_tokens: asr_audio_tokens_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_asr_context_sizing: asr_context_sizing_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            observation_asr_audio_tokens_source: asr_audio_tokens_source_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            observation_asr_beyond_model_audio_limit: asr_beyond_limit_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_draft_length: draft_length_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_dataset_subset: dataset_subset_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            parameter_batch_size: batch_size_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_temperature: temperature_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_top_p: top_p_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_top_k: top_k_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_min_p: min_p_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_presence_penalty: presence_penalty_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_repeat_penalty: repeat_penalty_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_seed: seed_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_prompt_profile: prompt_profile_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            parameter_num_beams: num_beams_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_beam_repetition_penalty: beam_repetition_penalty_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            parameter_beam_length_penalty: beam_length_penalty_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            benchmark_params: benchmark_params_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            benchmark_params_sha256: benchmark_params_sha256_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            device_time_ms: device_time_ms_col.and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_vl_spec_decode_turns: vl_spec_decode_turns_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_dataset_digest: dataset_digest_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
            observation_dataset_digest_matched: dataset_digest_matched_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            runtime_speculative_enabled: speculative_enabled_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i))),
            observation_output_digest: output_digest_col
                .and_then(|a| (!a.is_null(i)).then(|| a.value(i).to_string())),
        });
    }

    Ok(rows)
}

fn rows_to_batch(schema: &Arc<Schema>, rows: &[MetricRow]) -> anyhow::Result<RecordBatch> {
    let result_ids: Vec<&str> = rows.iter().map(|r| r.result_id.as_str()).collect();
    let benchmark_ids: Vec<&str> = rows.iter().map(|r| r.benchmark_id.as_str()).collect();
    let benchmark_types = rows
        .iter()
        .map(|r| enum_to_column(&r.benchmark_type))
        .collect::<anyhow::Result<Vec<String>>>()?;
    let metrics: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
    let client_ids: Vec<&str> = rows.iter().map(|r| r.client_id.as_str()).collect();
    let device_names: Vec<Option<&str>> = rows.iter().map(|r| r.device_name.as_deref()).collect();
    let device_form_factors = rows
        .iter()
        .map(|r| {
            r.device_form_factor
                .as_ref()
                .map(enum_to_column)
                .transpose()
        })
        .collect::<anyhow::Result<Vec<Option<String>>>>()?;
    let device_os_names: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_os_name.as_deref()).collect();
    let device_os_versions: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_os_version.as_deref())
        .collect();
    let device_os_builds: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_os_build.as_deref()).collect();
    let device_os_security_patches: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_os_security_patch.as_deref())
        .collect();
    let device_cpu_arches: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_cpu_arch.as_deref()).collect();
    let device_os_families: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_os_family.as_deref()).collect();
    let device_chip_models: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_chip_model.as_deref())
        .collect();
    let device_gpu_models: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_gpu_model.as_deref()).collect();
    let device_gpu_vram_bytes_col: Vec<Option<i64>> =
        rows.iter().map(|r| r.device_gpu_vram_bytes).collect();
    let device_npu_models: Vec<Option<&str>> =
        rows.iter().map(|r| r.device_npu_model.as_deref()).collect();
    let device_npu_vram_bytes_col: Vec<Option<i64>> =
        rows.iter().map(|r| r.device_npu_vram_bytes).collect();
    let device_ram_bytes_col: Vec<Option<i64>> = rows.iter().map(|r| r.device_ram_bytes).collect();
    let device_battery_level_col: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.device_battery_level.map(i32::from))
        .collect();
    let device_power_state_col = rows
        .iter()
        .map(|r| {
            r.device_power_state
                .as_ref()
                .map(enum_to_column)
                .transpose()
        })
        .collect::<anyhow::Result<Vec<Option<String>>>>()?;
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
    let device_apple_thermal_state_before_col =
        build_enum_list_array(rows, |r| r.device_apple_thermal_state_before.as_deref())?;
    let device_apple_thermal_state_after_col =
        build_enum_list_array(rows, |r| r.device_apple_thermal_state_after.as_deref())?;
    let device_apple_soc_temp_c_before_col: Vec<Option<Vec<f32>>> = rows
        .iter()
        .map(|r| r.device_apple_soc_temp_c_before.clone())
        .collect();
    let device_apple_soc_temp_c_after_col: Vec<Option<Vec<f32>>> = rows
        .iter()
        .map(|r| r.device_apple_soc_temp_c_after.clone())
        .collect();
    let device_android_thermal_status_before_col =
        build_enum_list_array(rows, |r| r.device_android_thermal_status_before.as_deref())?;
    let device_android_thermal_status_after_col =
        build_enum_list_array(rows, |r| r.device_android_thermal_status_after.as_deref())?;
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
    let values: Vec<Option<f32>> = rows.iter().map(|r| r.value).collect();
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
    let requested_model_flags: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.requested_model_flags.as_deref())
        .collect();
    let requested_model_flags_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.requested_model_flags_sha256.as_deref())
        .collect();
    let requested_runtime_flags: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.requested_runtime_flags.as_deref())
        .collect();
    let requested_runtime_flags_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.requested_runtime_flags_sha256.as_deref())
        .collect();
    let requested_benchmark_flags: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.requested_benchmark_flags.as_deref())
        .collect();
    let requested_benchmark_flags_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.requested_benchmark_flags_sha256.as_deref())
        .collect();
    let failed_col: Vec<Option<bool>> = rows.iter().map(|r| r.failed).collect();
    let failed_reason_col: Vec<Option<&str>> =
        rows.iter().map(|r| r.failed_reason.as_deref()).collect();
    let failure_source_col: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.failure_source.as_ref().map(|s| s.as_ref()))
        .collect();
    let failure_retriable_col: Vec<Option<bool>> =
        rows.iter().map(|r| r.failure_retriable).collect();

    let job_ids: Vec<Option<&str>> = rows.iter().map(|r| r.job_id.as_deref()).collect();
    let client_configurations: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.client_configuration.as_deref())
        .collect();
    let draft_lengths: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_draft_length).collect();
    let dataset_subsets: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.parameter_dataset_subset.as_deref())
        .collect();
    let vl_spec_decode_turns: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.observation_vl_spec_decode_turns)
        .collect();
    let dataset_digests: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.observation_dataset_digest.as_deref())
        .collect();
    let dataset_digest_matched: Vec<Option<bool>> = rows
        .iter()
        .map(|r| r.observation_dataset_digest_matched)
        .collect();
    let speculative_enabled: Vec<Option<bool>> =
        rows.iter().map(|r| r.runtime_speculative_enabled).collect();
    let output_digests: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.observation_output_digest.as_deref())
        .collect();
    let batch_sizes: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_batch_size).collect();
    let temperatures: Vec<Option<f64>> = rows.iter().map(|r| r.parameter_temperature).collect();
    let top_ps: Vec<Option<f64>> = rows.iter().map(|r| r.parameter_top_p).collect();
    let top_ks: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_top_k).collect();
    let min_ps: Vec<Option<f64>> = rows.iter().map(|r| r.parameter_min_p).collect();
    let seeds: Vec<Option<i64>> = rows.iter().map(|r| r.parameter_seed).collect();
    let client_extras: Vec<Option<&str>> =
        rows.iter().map(|r| r.client_extras.as_deref()).collect();
    let hardware_readings: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_hardware_readings.as_deref())
        .collect();
    let os_throttling_readings: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.device_os_throttling_readings.as_deref())
        .collect();
    let readiness_gate_waits: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.readiness_gate_waits.as_deref())
        .collect();
    let model_vault_descriptors: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.model_vault_descriptor.as_deref())
        .collect();
    let rescored_froms: Vec<Option<&str>> =
        rows.iter().map(|r| r.rescored_from.as_deref()).collect();
    let rerun_ofs: Vec<Option<&str>> = rows.iter().map(|r| r.rerun_of.as_deref()).collect();
    let benchmark_params: Vec<Option<&str>> =
        rows.iter().map(|r| r.benchmark_params.as_deref()).collect();
    let benchmark_params_sha256s: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.benchmark_params_sha256.as_deref())
        .collect();
    let device_times_ms: Vec<Option<i64>> = rows.iter().map(|r| r.device_time_ms).collect();
    let run_submitted_ats: Vec<Option<i64>> = rows.iter().map(|r| r.run_submitted_at).collect();
    let presence_penalties: Vec<Option<f64>> =
        rows.iter().map(|r| r.parameter_presence_penalty).collect();
    let repeat_penalties: Vec<Option<f64>> =
        rows.iter().map(|r| r.parameter_repeat_penalty).collect();
    let prompt_profiles: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.parameter_prompt_profile.as_deref())
        .collect();
    let audio_durations: Vec<Option<i32>> =
        rows.iter().map(|r| r.parameter_audio_duration_ms).collect();
    let audio_sample_rates: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.parameter_audio_sample_rate_hz)
        .collect();
    let audio_channels: Vec<Option<i32>> =
        rows.iter().map(|r| r.parameter_audio_channels).collect();
    let asr_prompt_tokens: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.observation_asr_throughput_prefill_tokens)
        .collect();
    let asr_audio_tokens: Vec<Option<i32>> = rows
        .iter()
        .map(|r| r.observation_asr_throughput_audio_tokens)
        .collect();
    let asr_context_sizings: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.observation_asr_context_sizing.as_deref())
        .collect();
    let asr_audio_tokens_sources: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.observation_asr_audio_tokens_source.as_deref())
        .collect();
    let asr_beyond_limits: Vec<Option<bool>> = rows
        .iter()
        .map(|r| r.observation_asr_beyond_model_audio_limit)
        .collect();
    let num_beams: Vec<Option<i32>> = rows.iter().map(|r| r.parameter_num_beams).collect();
    let beam_repetition_penalties: Vec<Option<f64>> = rows
        .iter()
        .map(|r| r.parameter_beam_repetition_penalty)
        .collect();
    let beam_length_penalties: Vec<Option<f64>> = rows
        .iter()
        .map(|r| r.parameter_beam_length_penalty)
        .collect();

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
            device_apple_thermal_state_before_col,
            device_apple_thermal_state_after_col,
            build_f32_list_array(&device_apple_soc_temp_c_before_col),
            build_f32_list_array(&device_apple_soc_temp_c_after_col),
            device_android_thermal_status_before_col,
            device_android_thermal_status_after_col,
            build_f32_list_array(&device_android_thermal_headroom_before_col),
            build_f32_list_array(&device_android_thermal_headroom_after_col),
            build_android_sensors_array(&device_android_thermal_sensors_before_col)?,
            build_android_sensors_array(&device_android_thermal_sensors_after_col)?,
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
            Arc::new(StringArray::from(requested_model_flags)),
            Arc::new(StringArray::from(requested_model_flags_sha256s)),
            Arc::new(StringArray::from(requested_runtime_flags)),
            Arc::new(StringArray::from(requested_runtime_flags_sha256s)),
            Arc::new(StringArray::from(requested_benchmark_flags)),
            Arc::new(StringArray::from(requested_benchmark_flags_sha256s)),
            Arc::new(BooleanArray::from(failed_col)),
            Arc::new(StringArray::from(failed_reason_col)),
            Arc::new(StringArray::from(failure_source_col)),
            Arc::new(BooleanArray::from(failure_retriable_col)),
            Arc::new(StringArray::from(job_ids)),
            Arc::new(StringArray::from(client_configurations)),
            Arc::new(Int32Array::from(draft_lengths)),
            Arc::new(StringArray::from(dataset_subsets)),
            Arc::new(Int32Array::from(vl_spec_decode_turns)),
            Arc::new(StringArray::from(dataset_digests)),
            Arc::new(BooleanArray::from(dataset_digest_matched)),
            Arc::new(BooleanArray::from(speculative_enabled)),
            Arc::new(StringArray::from(output_digests)),
            Arc::new(Int32Array::from(batch_sizes)),
            Arc::new(Float64Array::from(temperatures)),
            Arc::new(Float64Array::from(top_ps)),
            Arc::new(Int32Array::from(top_ks)),
            Arc::new(Float64Array::from(min_ps)),
            Arc::new(Int64Array::from(seeds)),
            Arc::new(StringArray::from(client_extras)),
            Arc::new(StringArray::from(hardware_readings)),
            Arc::new(StringArray::from(os_throttling_readings)),
            Arc::new(StringArray::from(readiness_gate_waits)),
            Arc::new(StringArray::from(model_vault_descriptors)),
            Arc::new(StringArray::from(rescored_froms)),
            Arc::new(StringArray::from(rerun_ofs)),
            Arc::new(StringArray::from(benchmark_params)),
            Arc::new(StringArray::from(benchmark_params_sha256s)),
            Arc::new(Int64Array::from(device_times_ms)),
            Arc::new(TimestampMicrosecondArray::from(run_submitted_ats).with_timezone("UTC")),
            Arc::new(Float64Array::from(presence_penalties)),
            Arc::new(Float64Array::from(repeat_penalties)),
            Arc::new(StringArray::from(prompt_profiles)),
            Arc::new(Int32Array::from(audio_durations)),
            Arc::new(Int32Array::from(audio_sample_rates)),
            Arc::new(Int32Array::from(audio_channels)),
            Arc::new(Int32Array::from(asr_prompt_tokens)),
            Arc::new(Int32Array::from(asr_audio_tokens)),
            Arc::new(StringArray::from(asr_context_sizings)),
            Arc::new(StringArray::from(asr_audio_tokens_sources)),
            Arc::new(BooleanArray::from(asr_beyond_limits)),
            Arc::new(StringArray::from(device_cpu_arches)),
            Arc::new(StringArray::from(device_os_families)),
            Arc::new(Int32Array::from(num_beams)),
            Arc::new(Float64Array::from(beam_repetition_penalties)),
            Arc::new(Float64Array::from(beam_length_penalties)),
        ],
    )?;

    Ok(batch)
}

pub struct JobMetrics {
    pub scored_at: chrono::DateTime<chrono::Utc>,
    pub score_runtime_version: Option<String>,
    pub metrics: Vec<JobMetric>,
}

impl JobMetrics {
    /// Build a job's metrics from the **latest scoring run** among `rows`, or
    /// `None` if `rows` is empty.
    ///
    /// A crash before `mark_processed` re-scores and appends a fresh copy; one
    /// run's rows share a `scored_at`, so keeping the max drops older copies
    /// wholesale. Rows with no `value` (failure rows) are dropped: a device's
    /// job read carries a bare `f32`.
    pub fn from_latest_rows(rows: &[MetricRow]) -> Option<Self> {
        let latest_scored_at = rows.iter().map(|r| r.scored_at).max()?;
        let latest: Vec<&MetricRow> = rows
            .iter()
            .filter(|r| r.scored_at == latest_scored_at)
            .collect();
        let scored_at = chrono::DateTime::from_timestamp_micros(latest_scored_at)?;
        let metrics = latest
            .iter()
            .filter_map(|r| {
                Some(JobMetric {
                    metric: r.metric.clone(),
                    value: r.value?,
                    value_stddev: r.value_stddev,
                    unit: r.unit.clone(),
                })
            })
            .collect();
        Some(Self {
            scored_at,
            score_runtime_version: latest.first().and_then(|r| r.score_runtime_version.clone()),
            metrics,
        })
    }
}

/// Read metrics for a specific job from Parquet files.
///
/// Scans partitions within `read_days` (a hard cap), `day=` before `month=`,
/// and stops at the first match, which holds the newest copy.
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
        // Order is irrelevant: `from_latest_rows` picks the newest copy.
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
        if !matching.is_empty() {
            return Ok(JobMetrics::from_latest_rows(&matching));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use rstest::rstest;
    use strum::IntoEnumIterator;

    use pipette_plan_types::AndroidThrottlingSeverity;

    use crate::warehouse::*;

    /// The reserved id parses, so the server can record under it, and only it is the system.
    #[rstest]
    #[case::reserved(SYSTEM_CLIENT_ID, true)]
    #[case::padded("  system  ", true)]
    #[case::a_device("ev1_a3f8c2", false)]
    #[case::cased("System", false)]
    fn only_the_reserved_client_id_is_the_system(
        #[case] raw: &str,
        #[case] system: bool,
    ) -> anyhow::Result<()> {
        assert_eq!(
            is_system_client(&ClientId::try_new(raw)?),
            system,
            "{raw:?}"
        );
        Ok(())
    }

    /// A crash between write and rename strands the staged file, so its name
    /// must be invisible to a `.parquet` reader; a round trip cannot see it.
    #[test]
    fn parquet_staging_path_is_invisible_to_a_parquet_reader() {
        let partition = Path::new("/w/benchmark_id=b/client_id=c/day=2025-03-10");
        let staged = parquet_staging_path(partition);

        assert_eq!(staged.parent(), Some(partition));
        assert_ne!(
            staged.extension().and_then(|e| e.to_str()),
            Some("parquet"),
            "a `.parquet` staging name is read back as a part file: {}",
            staged.display()
        );
        assert!(
            staged
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.starts_with(".tmp-")),
            "the stem carries the `.tmp-` prefix the readers skip on: {}",
            staged.display()
        );
    }

    /// Two calls never collide, so concurrent writers into one partition cannot
    /// stage over each other.
    #[test]
    fn parquet_staging_path_is_unique_per_call() {
        let partition = Path::new("/w/day=2025-03-10");
        assert_ne!(
            parquet_staging_path(partition),
            parquet_staging_path(partition)
        );
    }

    /// The lock window runs from midnight UTC of the partition's day, so 72
    /// hours leave exactly three day partitions open at any time of day.
    #[rstest]
    // `now` is late in the day, so yesterday's body is ~31h old and the
    // day-before-yesterday's is ~55h — both well inside 72h.
    #[case::submitted_now("2026-08-27T23:00:00Z", "2026-08-27T23:00:00Z", true)]
    #[case::yesterday("2026-08-26T16:00:00Z", "2026-08-27T23:00:00Z", true)]
    #[case::two_days_back("2026-08-25T16:00:00Z", "2026-08-27T23:00:00Z", true)]
    // Three days back closed at 2026-08-28T00:00Z; `now` is before that, so it
    // is still open by an hour. This is the widest the window ever gets.
    #[case::three_days_back_still_open("2026-08-25T00:00:00Z", "2026-08-27T23:00:00Z", true)]
    #[case::three_days_back_just_closed("2026-08-24T23:59:59Z", "2026-08-27T00:00:00Z", false)]
    // Submitted minutes before midnight, read minutes after: only ~4 hours old
    // in wall-clock terms, but its partition day is already three days back and
    // the whole partition closes together.
    #[case::young_body_in_a_closed_partition("2026-08-24T23:50:00Z", "2026-08-28T03:00:00Z", false)]
    #[case::long_past("2026-03-01T12:00:00Z", "2026-08-27T12:00:00Z", false)]
    fn day_partition_lock_window_counts_from_midnight_of_its_own_day(
        #[case] submitted_at: &str,
        #[case] now: &str,
        #[case] want_open: bool,
    ) -> anyhow::Result<()> {
        let submitted_at = submitted_at.parse::<chrono::DateTime<chrono::Utc>>()?;
        let now = now.parse::<chrono::DateTime<chrono::Utc>>()?;
        assert_eq!(
            day_partition_is_open(submitted_at, now, chrono::TimeDelta::hours(72)),
            want_open
        );
        Ok(())
    }

    /// A body from a client clock running ahead is open: refusing it would
    /// strand it in `incoming/` for as long as the skew lasts.
    #[test]
    fn day_partition_ahead_of_now_is_open() -> anyhow::Result<()> {
        assert!(day_partition_is_open(
            "2026-08-29T00:00:00Z".parse()?,
            "2026-08-27T12:00:00Z".parse()?,
            chrono::TimeDelta::hours(72),
        ));
        Ok(())
    }

    fn make_test_row(metric: &str, value: f32, unit: &str) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            metric: metric.to_string(),
            value: Some(value),
            unit: unit.to_string(),
            submitted_at: 1_000_000,
            scored_at: 2_000_000,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    /// The HTTP boundary parses these with `FromStr` or `as_wire_str`, the
    /// warehouse with serde; both must spell every variant the same way.
    #[test]
    fn enum_columns_use_the_spelling_the_boundary_parses() -> anyhow::Result<()> {
        DeviceFormFactor::iter().try_for_each(|v| -> anyhow::Result<()> {
            let column = enum_to_column(&v)?;
            assert_eq!(column.parse::<DeviceFormFactor>()?, v);
            assert_eq!(enum_from_column::<DeviceFormFactor>(&column)?, v);
            Ok(())
        })?;
        [
            DevicePowerState::Charging,
            DevicePowerState::NotCharging,
            DevicePowerState::PluggedInNotCharging,
        ]
        .iter()
        .try_for_each(|v| -> anyhow::Result<()> {
            assert_eq!(enum_to_column(v)?, v.as_wire_str());
            Ok(())
        })?;
        assert!(enum_from_column::<AppleThermalState>("Serious").is_err());
        Ok(())
    }

    #[test]
    fn test_device_form_factor_from_str() -> anyhow::Result<()> {
        DeviceFormFactor::iter().try_for_each(|v| -> anyhow::Result<()> {
            assert_eq!(v.to_string().parse::<DeviceFormFactor>()?, v);
            Ok(())
        })?;
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
        assert_eq!(rows[0].device_ram_bytes, Some(17_179_869_184));
        assert_eq!(rows[0].device_chip_model.as_deref(), Some("test-chip"));
        assert_eq!(rows[0].device_form_factor, Some(DeviceFormFactor::Embedded));
        Ok(())
    }

    #[test]
    fn test_os_build_security_patch_round_trip() -> anyhow::Result<()> {
        // Row 0 populates both fields; row 1 leaves them `None` so the
        // `Some`/`None` split survives the array round-trip.
        let mut populated = make_test_row("ttft", 34.7, "ms")?;
        populated.device_os_build = Some("AP3A.240905.015.A2".to_string());
        populated.device_os_security_patch = Some("2025-06-01".to_string());
        populated.device_cpu_arch = Some("aarch64".to_string());
        populated.device_os_family = Some("Linux".to_string());

        let empty = make_test_row("ttft", 30.1, "ms")?;
        assert_eq!(empty.device_os_build, None);
        assert_eq!(empty.device_os_security_patch, None);
        assert_eq!(empty.device_cpu_arch, None);
        assert_eq!(empty.device_os_family, None);

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
        assert_eq!(rows[0].device_cpu_arch.as_deref(), Some("aarch64"));
        assert_eq!(rows[0].device_os_family.as_deref(), Some("Linux"));
        assert_eq!(rows[1].device_cpu_arch, None);
        assert_eq!(rows[1].device_os_family, None);
        Ok(())
    }

    /// Write `rows` to a fresh parquet file and read them back.
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
        // Three rows cover every `DevicePowerState` and both `Some`/`None`.
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
        // A Samsung-style demotion and a Pixel-style row cover both states.
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
    fn test_asr_throughput_columns_round_trip() -> anyhow::Result<()> {
        let mut asr = make_test_row("ttft", 83.3, "ms")?;
        asr.parameter_audio_duration_ms = Some(9_400);
        asr.parameter_audio_sample_rate_hz = Some(16_000);
        asr.parameter_audio_channels = Some(1);
        asr.observation_asr_throughput_prefill_tokens = Some(137);
        asr.observation_asr_throughput_audio_tokens = Some(118);
        asr.observation_asr_context_sizing = Some("exact".to_string());
        asr.observation_asr_audio_tokens_source = Some("calibrated".to_string());
        asr.observation_asr_beyond_model_audio_limit = Some(true);
        let other = make_test_row("decode_throughput", 42.0, "tokens/sec")?;
        let rows = round_trip(&[asr, other])?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].parameter_audio_duration_ms, Some(9_400));
        assert_eq!(rows[0].parameter_audio_sample_rate_hz, Some(16_000));
        assert_eq!(rows[0].parameter_audio_channels, Some(1));
        assert_eq!(rows[0].observation_asr_throughput_prefill_tokens, Some(137));
        assert_eq!(rows[0].observation_asr_throughput_audio_tokens, Some(118));
        assert_eq!(
            rows[0].observation_asr_context_sizing.as_deref(),
            Some("exact")
        );
        assert_eq!(
            rows[0].observation_asr_audio_tokens_source.as_deref(),
            Some("calibrated")
        );
        assert_eq!(rows[0].observation_asr_beyond_model_audio_limit, Some(true));
        assert_eq!(rows[1].parameter_audio_duration_ms, None);
        assert_eq!(rows[1].parameter_audio_sample_rate_hz, None);
        assert_eq!(rows[1].parameter_audio_channels, None);
        assert_eq!(rows[1].observation_asr_throughput_prefill_tokens, None);
        assert_eq!(rows[1].observation_asr_throughput_audio_tokens, None);
        assert_eq!(rows[1].observation_asr_context_sizing, None);
        assert_eq!(rows[1].observation_asr_audio_tokens_source, None);
        assert_eq!(rows[1].observation_asr_beyond_model_audio_limit, None);
        Ok(())
    }

    /// A file written before the ASR columns existed still reads back, the
    /// columns absent and not null within it, and appending a row that carries
    /// them backfills the columns without losing the old row.
    #[test]
    fn test_asr_columns_none_when_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("ttft", 83.3, "ms")?;
        new_row.result_id = "job2_0".to_string();
        new_row.parameter_audio_duration_ms = Some(9_400);
        new_row.parameter_audio_sample_rate_hz = Some(48_000);
        new_row.parameter_audio_channels = Some(2);
        new_row.observation_asr_throughput_prefill_tokens = Some(137);
        new_row.observation_asr_throughput_audio_tokens = Some(118);
        new_row.observation_asr_context_sizing = Some("upper_bound".to_string());
        new_row.observation_asr_audio_tokens_source = Some("measured".to_string());
        new_row.observation_asr_beyond_model_audio_limit = Some(false);
        let rows = append_to_file_without_columns(
            &[
                "parameter_audio_duration_ms",
                "parameter_audio_sample_rate_hz",
                "parameter_audio_channels",
                "observation_asr_throughput_prefill_tokens",
                "observation_asr_throughput_audio_tokens",
                "observation_asr_context_sizing",
                "observation_asr_audio_tokens_source",
                "observation_asr_beyond_model_audio_limit",
            ],
            new_row,
        )?;
        assert_eq!(rows.len(), 2);
        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(old.parameter_audio_duration_ms, None);
        assert_eq!(old.parameter_audio_sample_rate_hz, None);
        assert_eq!(old.parameter_audio_channels, None);
        assert_eq!(old.observation_asr_throughput_prefill_tokens, None);
        assert_eq!(old.observation_asr_throughput_audio_tokens, None);
        assert_eq!(old.observation_asr_context_sizing, None);
        assert_eq!(old.observation_asr_audio_tokens_source, None);
        assert_eq!(old.observation_asr_beyond_model_audio_limit, None);
        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(new.parameter_audio_duration_ms, Some(9_400));
        assert_eq!(new.parameter_audio_sample_rate_hz, Some(48_000));
        assert_eq!(new.parameter_audio_channels, Some(2));
        assert_eq!(new.observation_asr_throughput_prefill_tokens, Some(137));
        assert_eq!(new.observation_asr_throughput_audio_tokens, Some(118));
        assert_eq!(
            new.observation_asr_context_sizing.as_deref(),
            Some("upper_bound")
        );
        assert_eq!(
            new.observation_asr_audio_tokens_source.as_deref(),
            Some("measured")
        );
        assert_eq!(new.observation_asr_beyond_model_audio_limit, Some(false));
        Ok(())
    }

    #[test]
    fn test_vl_throughput_observation_columns_round_trip() -> anyhow::Result<()> {
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
    fn test_vl_spec_decode_columns_round_trip() -> anyhow::Result<()> {
        let mut spec = make_test_row("accept_length", 4.04, "tokens")?;
        spec.parameter_draft_length = Some(8);
        spec.parameter_dataset_subset = Some("chart_qa".to_string());
        spec.observation_vl_spec_decode_turns = Some(723);
        spec.observation_dataset_digest = Some("ab".repeat(32));
        spec.observation_dataset_digest_matched = Some(true);
        spec.runtime_speculative_enabled = Some(false);
        spec.observation_output_digest = Some("cd".repeat(32));
        spec.parameter_batch_size = Some(8);
        let other = make_test_row("decode_throughput", 42.0, "tokens/sec")?;

        let rows = round_trip(&[spec, other])?;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].parameter_draft_length, Some(8));
        assert_eq!(
            rows[0].parameter_dataset_subset.as_deref(),
            Some("chart_qa")
        );
        assert_eq!(rows[0].observation_vl_spec_decode_turns, Some(723));
        assert_eq!(rows[0].observation_dataset_digest_matched, Some(true));
        assert_eq!(
            rows[0].observation_dataset_digest.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        // `Some(false)` must survive as false, not collapse into NULL.
        assert_eq!(rows[0].runtime_speculative_enabled, Some(false));
        assert_eq!(
            rows[0].observation_output_digest.as_deref(),
            Some("cd".repeat(32).as_str())
        );
        assert_eq!(rows[0].parameter_batch_size, Some(8));
        assert_eq!(rows[1].parameter_draft_length, None);
        assert_eq!(rows[1].parameter_batch_size, None);
        assert_eq!(rows[1].observation_dataset_digest, None);
        assert_eq!(rows[1].runtime_speculative_enabled, None);
        assert_eq!(rows[1].observation_vl_spec_decode_turns, None);
        assert_eq!(rows[1].observation_output_digest, None);
        Ok(())
    }

    /// A file written before the corpus columns existed still reads back, with
    /// them null — the append-only rule in `docs/storage.md`.
    #[test]
    fn test_vl_spec_decode_none_when_columns_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accept_length", 4.04, "tokens")?;
        new_row.result_id = "job2_0".to_string();
        new_row.parameter_draft_length = Some(8);
        new_row.observation_dataset_digest_matched = Some(true);
        new_row.runtime_speculative_enabled = Some(true);
        new_row.observation_vl_spec_decode_turns = Some(723);
        new_row.observation_output_digest = Some("cd".repeat(32));
        new_row.parameter_batch_size = Some(8);

        let rows = append_to_file_without_columns(
            &[
                "parameter_draft_length",
                "parameter_dataset_subset",
                "observation_vl_spec_decode_turns",
                "observation_dataset_digest",
                "observation_dataset_digest_matched",
                "runtime_speculative_enabled",
                "observation_output_digest",
                "parameter_batch_size",
            ],
            new_row,
        )?;

        assert_eq!(rows.len(), 2);
        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(old.parameter_draft_length, None);
        assert_eq!(old.observation_vl_spec_decode_turns, None);
        assert_eq!(old.observation_dataset_digest_matched, None);
        assert_eq!(old.runtime_speculative_enabled, None);
        assert_eq!(old.observation_output_digest, None);
        assert_eq!(old.parameter_batch_size, None);
        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(new.parameter_draft_length, Some(8));
        assert_eq!(new.parameter_batch_size, Some(8));
        assert_eq!(new.runtime_speculative_enabled, Some(true));
        assert_eq!(new.observation_vl_spec_decode_turns, Some(723));
        Ok(())
    }

    #[test]
    fn eval_sampling_columns_round_trip() -> anyhow::Result<()> {
        // `0.0` and `0` are pins: they must come back as values rather than
        // collapsing into the null that means "unpinned".
        let mut pinned = make_test_row("accuracy", 0.61, "ratio")?;
        pinned.parameter_temperature = Some(0.0);
        pinned.parameter_top_p = Some(0.95);
        pinned.parameter_top_k = Some(0);
        pinned.parameter_min_p = Some(0.05);
        pinned.parameter_presence_penalty = Some(0.0);
        pinned.parameter_repeat_penalty = Some(1.05);
        pinned.parameter_seed = Some(4_294_967_296);
        let unpinned = make_test_row("decode_throughput", 42.0, "tokens/sec")?;

        let rows = round_trip(&[pinned, unpinned])?;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].parameter_temperature, Some(0.0));
        // Exactly the `f64` the catalog held: `Float32` would round it.
        assert_eq!(rows[0].parameter_top_p, Some(0.95));
        assert_eq!(rows[0].parameter_top_k, Some(0));
        assert_eq!(rows[0].parameter_min_p, Some(0.05));
        assert_eq!(rows[0].parameter_presence_penalty, Some(0.0));
        assert_eq!(rows[0].parameter_repeat_penalty, Some(1.05));
        assert_eq!(rows[0].parameter_seed, Some(4_294_967_296));
        assert_eq!(rows[1].parameter_temperature, None);
        assert_eq!(rows[1].parameter_top_p, None);
        assert_eq!(rows[1].parameter_top_k, None);
        assert_eq!(rows[1].parameter_min_p, None);
        assert_eq!(rows[1].parameter_presence_penalty, None);
        assert_eq!(rows[1].parameter_repeat_penalty, None);
        assert_eq!(rows[1].parameter_seed, None);
        Ok(())
    }

    /// A file written before the penalty columns existed still reads back,
    /// with them null.
    #[test]
    fn penalties_none_when_columns_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.61, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.parameter_presence_penalty = Some(1.5);
        new_row.parameter_repeat_penalty = Some(1.05);

        let rows = append_to_file_without_columns(
            &["parameter_presence_penalty", "parameter_repeat_penalty"],
            new_row,
        )?;

        assert_eq!(rows.len(), 2);
        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(old.parameter_presence_penalty, None);
        assert_eq!(old.parameter_repeat_penalty, None);
        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(new.parameter_presence_penalty, Some(1.5));
        assert_eq!(new.parameter_repeat_penalty, Some(1.05));
        Ok(())
    }

    /// A file written before the prompt-profile column existed still reads
    /// back, with it null.
    #[test]
    fn prompt_profile_none_when_column_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.61, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.parameter_prompt_profile = Some("internvl".to_string());

        let rows = append_to_file_without_columns(&["parameter_prompt_profile"], new_row)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(row_by_id(&rows, "job1_0")?.parameter_prompt_profile, None);
        assert_eq!(
            row_by_id(&rows, "job2_0")?
                .parameter_prompt_profile
                .as_deref(),
            Some("internvl")
        );
        Ok(())
    }

    /// A file written before the beam columns existed still reads back, with
    /// them null.
    #[test]
    fn beam_columns_none_when_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("wer", 0.05, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.parameter_num_beams = Some(3);
        new_row.parameter_beam_repetition_penalty = Some(1.2);
        new_row.parameter_beam_length_penalty = Some(1.0);

        let rows = append_to_file_without_columns(
            &[
                "parameter_num_beams",
                "parameter_beam_repetition_penalty",
                "parameter_beam_length_penalty",
            ],
            new_row,
        )?;

        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(
            (
                old.parameter_num_beams,
                old.parameter_beam_repetition_penalty,
                old.parameter_beam_length_penalty
            ),
            (None, None, None)
        );
        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(
            (
                new.parameter_num_beams,
                new.parameter_beam_repetition_penalty,
                new.parameter_beam_length_penalty
            ),
            (Some(3), Some(1.2), Some(1.0))
        );
        Ok(())
    }

    /// A file written before the sampling columns existed still reads back,
    /// with them null — the append-only rule in `docs/storage.md`.
    #[test]
    fn eval_sampling_none_when_columns_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.61, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.parameter_temperature = Some(1.0);
        new_row.parameter_top_p = Some(0.95);
        new_row.parameter_top_k = Some(64);
        new_row.parameter_min_p = Some(0.0);
        new_row.parameter_seed = Some(7);

        let rows = append_to_file_without_columns(
            &[
                "parameter_temperature",
                "parameter_top_p",
                "parameter_top_k",
                "parameter_min_p",
                "parameter_seed",
            ],
            new_row,
        )?;

        assert_eq!(rows.len(), 2);
        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(old.parameter_temperature, None);
        assert_eq!(old.parameter_top_p, None);
        assert_eq!(old.parameter_top_k, None);
        assert_eq!(old.parameter_min_p, None);
        assert_eq!(old.parameter_seed, None);
        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(new.parameter_temperature, Some(1.0));
        assert_eq!(new.parameter_top_k, Some(64));
        assert_eq!(new.parameter_seed, Some(7));
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
            value: Some(value),
            unit: unit.to_string(),
            submitted_at: 1_000_000,
            scored_at,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    #[test]
    fn test_append_to_file_missing_newer_nullable_columns() -> anyhow::Result<()> {
        // An older file: newer nullable columns missing, and `value`,
        // `device_*`, `model_*`, `runtime_*` declared `nullable: false`, as
        // before the failure-row widening. Reading and appending must work.
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
        // The old file declares `value` and the device columns `nullable: false`
        // and they still read back populated.
        assert_eq!(rows[0].value, Some(34.7));
        assert_eq!(rows[0].device_name.as_deref(), Some("test-device"));
        assert_eq!(rows[0].device_ram_bytes, Some(17_179_869_184));
        assert_eq!(rows[0].device_form_factor, Some(DeviceFormFactor::Embedded));
        // The old file predates these columns; the lenient read yields `None`.
        assert_eq!(rows[0].device_os_build, None);
        assert_eq!(rows[0].device_os_security_patch, None);
        assert_eq!(rows[0].device_cpu_arch, None);
        assert_eq!(rows[0].device_os_family, None);
        // Likewise the failure columns.
        assert_eq!(rows[0].failed, None);
        assert_eq!(rows[0].failed_reason, None);
        assert_eq!(rows[0].failure_source, None);
        assert_eq!(rows[0].failure_retriable, None);
        // `job_id` is the same case: a row from before the column reads NULL, so
        // a per-cell tally over a table spanning the boundary must expect it.
        assert_eq!(rows[0].job_id, None);
        // The appended row carries them.
        assert_eq!(rows[1].failed, Some(false));
        assert_eq!(rows[1].job_id.as_deref(), Some("r1"));
        Ok(())
    }

    /// `job_id` round-trips its own value: a misplaced entry in the positional
    /// schema still compiles and round-trips a string, just the wrong one.
    #[test]
    fn test_job_id_column_round_trips_its_own_value() -> anyhow::Result<()> {
        let rows = [
            MetricRow {
                result_id: "550e8400-job-a_0".to_string(),
                job_id: Some("550e8400-job-a".to_string()),
                failure_source: Some(FailureSource::Expired),
                failure_retriable: Some(true),
                failed: Some(true),
                failed_reason: Some("expired".to_string()),
                ..Default::default()
            },
            // A row with no `job_id`, so the null path through the array builder
            // is exercised beside the populated one.
            MetricRow {
                result_id: "550e8400-job-b_0".to_string(),
                job_id: None,
                ..Default::default()
            },
        ];

        let back = round_trip(&rows)?;

        assert_eq!(back.len(), 2);
        assert_eq!(back[0].job_id.as_deref(), Some("550e8400-job-a"));
        assert_eq!(back[0].result_id, "550e8400-job-a_0");
        // The columns either side of `job_id` in the schema keep their own values.
        assert_eq!(back[0].failure_retriable, Some(true));
        assert_eq!(back[0].failure_source, Some(FailureSource::Expired));
        assert_eq!(back[1].job_id, None);
        assert_eq!(back[1].result_id, "550e8400-job-b_0");
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
        assert!(result.scored_at.timestamp() > 0);
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
        // `eval_metadata` round-trips losslessly, and `None` reads back as
        // `None`, not an empty string.
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

    /// Every nullable string column round-trips a value verbatim and absence
    /// as `None`, not an empty string.
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

    /// Write one row of the current batch with `absent` projected away, append
    /// `new_row` (give it a `result_id` other than `"job1_0"`), and read all
    /// rows back.
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

    /// A file without `client_version` still reads back, and an appended row
    /// backfills the column without losing the old row.
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

    /// A file without the memory-observation columns still reads back, and an
    /// appended row backfills them without losing the old row.
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

    /// A 64-bit `MetricRow` field, so one test can run over several columns.
    type I64Field = fn(&mut MetricRow) -> &mut Option<i64>;

    /// A file without the column reads back with NULL in it. An appended row
    /// that carries a value backfills the column.
    #[rstest]
    #[case::device_time_ms("device_time_ms", 184_250, (|r| &mut r.device_time_ms) as I64Field)]
    #[case::run_submitted_at(
        "run_submitted_at",
        1_780_000_000_000_000,
        (|r| &mut r.run_submitted_at) as I64Field
    )]
    fn test_i64_column_none_when_absent(
        #[case] column: &str,
        #[case] value: i64,
        #[case] field: I64Field,
    ) -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.9, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        *field(&mut new_row) = Some(value);

        let rows = append_to_file_without_columns(&[column], new_row)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(*field(&mut row_by_id(&rows, "job1_0")?), None);
        assert_eq!(*field(&mut row_by_id(&rows, "job2_0")?), Some(value));
        Ok(())
    }

    /// A file without the six requested-flag columns reads back as "no
    /// request recorded", and an appended row backfills them.
    #[test]
    fn test_requested_flags_none_when_columns_absent() -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.9, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        new_row.requested_model_flags = Some(r#"{"model_type":"gguf_text"}"#.to_string());
        new_row.requested_model_flags_sha256 = Some("mf".to_string());
        new_row.requested_runtime_flags = Some(r#"{"threads":8}"#.to_string());
        new_row.requested_runtime_flags_sha256 = Some("rf".to_string());
        new_row.requested_benchmark_flags = Some(r#"{"readiness":{}}"#.to_string());
        new_row.requested_benchmark_flags_sha256 = Some("bf".to_string());

        let rows = append_to_file_without_columns(
            &[
                "requested_model_flags",
                "requested_model_flags_sha256",
                "requested_runtime_flags",
                "requested_runtime_flags_sha256",
                "requested_benchmark_flags",
                "requested_benchmark_flags_sha256",
            ],
            new_row,
        )?;

        assert_eq!(rows.len(), 2);
        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(old.requested_model_flags, None);
        assert_eq!(old.requested_model_flags_sha256, None);
        assert_eq!(old.requested_runtime_flags, None);
        assert_eq!(old.requested_runtime_flags_sha256, None);
        assert_eq!(old.requested_benchmark_flags, None);
        assert_eq!(old.requested_benchmark_flags_sha256, None);

        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(
            new.requested_model_flags.as_deref(),
            Some(r#"{"model_type":"gguf_text"}"#)
        );
        assert_eq!(new.requested_model_flags_sha256.as_deref(), Some("mf"));
        assert_eq!(
            new.requested_runtime_flags.as_deref(),
            Some(r#"{"threads":8}"#)
        );
        assert_eq!(new.requested_runtime_flags_sha256.as_deref(), Some("rf"));
        assert_eq!(
            new.requested_benchmark_flags.as_deref(),
            Some(r#"{"readiness":{}}"#)
        );
        assert_eq!(new.requested_benchmark_flags_sha256.as_deref(), Some("bf"));
        Ok(())
    }

    /// A file written before the pair existed reads NULL for both, and a row
    /// carrying them lands each in its own column.
    #[test]
    fn test_benchmark_params_none_when_columns_absent() -> anyhow::Result<()> {
        let params =
            r#"{"batch_size":1,"benchmark_type":"prefill_throughput","prefill_tokens":256}"#;
        let mut new_row = make_test_row("prefill_throughput", 900.0, "tokens/sec")?;
        new_row.result_id = "job2_0".to_string();
        new_row.benchmark_params = Some(params.to_string());
        new_row.benchmark_params_sha256 = Some("bp".to_string());

        let rows = append_to_file_without_columns(
            &["benchmark_params", "benchmark_params_sha256"],
            new_row,
        )?;

        assert_eq!(rows.len(), 2);
        let old = row_by_id(&rows, "job1_0")?;
        assert_eq!(old.benchmark_params, None);
        assert_eq!(old.benchmark_params_sha256, None);
        let new = row_by_id(&rows, "job2_0")?;
        assert_eq!(new.benchmark_params.as_deref(), Some(params));
        assert_eq!(new.benchmark_params_sha256.as_deref(), Some("bp"));
        Ok(())
    }

    /// `batch_to_rows` reads an unpadded old batch, as `tail_part` does, so the
    /// pair reads NULL there too.
    #[test]
    fn test_benchmark_params_none_in_a_batch_without_them() -> anyhow::Result<()> {
        let mut row = make_test_row("prefill_throughput", 900.0, "tokens/sec")?;
        row.benchmark_params = Some("{}".to_string());
        row.benchmark_params_sha256 = Some("bp".to_string());
        let batch = rows_to_batch(&Arc::new(parquet_schema()), &[row])?;
        let keep: Vec<usize> = (0..batch.num_columns())
            .filter(|&i| {
                !batch
                    .schema()
                    .field(i)
                    .name()
                    .starts_with("benchmark_params")
            })
            .collect();
        let old = batch.project(&keep)?;
        assert_eq!(old.num_columns(), batch.num_columns() - 2);

        let rows = batch_to_rows(&old)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].benchmark_params, None);
        assert_eq!(rows[0].benchmark_params_sha256, None);
        Ok(())
    }

    /// A `MetricRow` field, so one test can run over several columns.
    type RowField = fn(&mut MetricRow) -> &mut Option<String>;

    /// The whole migration for a verbatim column: a file written before it
    /// reads NULL, and an appended row carrying one backfills it byte for byte,
    /// spacing included.
    #[rstest]
    #[case::client_configuration(
        "client_configuration",
        r#"{ "envs": [ "CUDA_VISIBLE_DEVICES", "GGML_CUDA_ENABLE_UNIFIED_MEMORY=0" ] }"#,
        (|r| &mut r.client_configuration) as RowField
    )]
    #[case::client_extras(
        "client_extras",
        r#"{ "build": "nightly", "retries": 2 }"#,
        (|r| &mut r.client_extras) as RowField
    )]
    #[case::device_hardware_readings(
        "device_hardware_readings",
        r#"[ {"iteration": 0, "sample_point": "before_rep", "linux_sysfs_thermal_zones": [{"zone": "thermal_zone0", "type": "cpu-thermal", "temp": 62300}]} ]"#,
        (|r| &mut r.device_hardware_readings) as RowField
    )]
    #[case::device_os_throttling_readings(
        "device_os_throttling_readings",
        r#"[ {"iteration": 0, "sample_point": "before_rep", "apple_process_info_thermal_state": 0} ]"#,
        (|r| &mut r.device_os_throttling_readings) as RowField
    )]
    #[case::readiness_gate_waits(
        "readiness_gate_waits",
        r#"[ {"iteration": 0, "poll_count": 1, "phase": "proceeding", "elapsed_ms": 38} ]"#,
        (|r| &mut r.readiness_gate_waits) as RowField
    )]
    #[case::model_vault_descriptor(
        "model_vault_descriptor",
        r#"{"principal_id":"org_1","source":"vault_url","type":"gguf_text","url":"/vault/resources/vault_0199aaaa-bbbb-7ccc-8ddd-000000000001/content"}"#,
        (|r| &mut r.model_vault_descriptor) as RowField
    )]
    #[case::rescored_from(
        "rescored_from",
        "0199aaaa-bbbb-7ccc-8ddd-000000000001",
        (|r| &mut r.rescored_from) as RowField
    )]
    #[case::rerun_of(
        "rerun_of",
        "0199aaaa-bbbb-7ccc-8ddd-000000000002",
        (|r| &mut r.rerun_of) as RowField
    )]
    fn test_verbatim_column_none_when_absent(
        #[case] column: &str,
        #[case] wire: &str,
        #[case] field: RowField,
    ) -> anyhow::Result<()> {
        let mut new_row = make_test_row("accuracy", 0.9, "ratio")?;
        new_row.result_id = "job2_0".to_string();
        *field(&mut new_row) = Some(wire.to_string());

        let rows = append_to_file_without_columns(&[column], new_row)?;

        assert_eq!(rows.len(), 2);
        assert_eq!(*field(&mut row_by_id(&rows, "job1_0")?), None);
        assert_eq!(
            field(&mut row_by_id(&rows, "job2_0")?).as_deref(),
            Some(wire)
        );
        Ok(())
    }
}
