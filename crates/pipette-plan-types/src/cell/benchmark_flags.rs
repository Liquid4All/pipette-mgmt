//! Per-cell run-driving knobs: one [`BenchmarkFlags`] variant per supported
//! `(benchmark, runtime_type, model)` cell, carrying exactly the knobs that
//! cell accepts. Authored flat via [`BenchmarkFlagRef`].

use serde::{Deserialize, Serialize};

use crate::{BenchmarkType, Model, ModelType, PositiveSeconds, Runtime, RuntimeType, SkinCelsius};

/// Host-readiness gate settings for a cell: how long to wait for the device to
/// reach a nominal thermal/quiet state before a measurement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ReadinessOverrides {
    /// Override the per-platform readiness deadline, in whole seconds. `None`
    /// keeps the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_secs: Option<u64>,
    /// Waive the platform's *thermal* criterion for this cell, keeping the load
    /// criterion. `None`/`false` enforces it. For hosts where the thermal
    /// signal costs more than it buys (macOS's is a fixed ~318 s hold-off, not
    /// a temperature). Results that skipped it are not comparable to ones that
    /// didn't, so set it per cell, not fleet-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_thermal: Option<bool>,
    /// Android CLI gate: release a rep only below this skin temperature, °C.
    /// Unset leaves the skin criterion off. Other platforms refuse a set value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin_below_c: Option<SkinCelsius>,
}

/// Resolving a [`BenchmarkFlagRef`] into a [`BenchmarkFlags`] failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BenchmarkFlagsError {
    /// The `(benchmark, runtime, model)` triple names no flag-carrying cell.
    #[error("no benchmark flags defined for {model:?} running {benchmark:?} on {runtime:?}")]
    NoSuchCombination {
        benchmark: BenchmarkType,
        runtime: RuntimeType,
        model: ModelType,
    },
    /// A knob was set on a cell whose variant has no field for it.
    #[error(
        "{knob} is not a valid benchmark flag for {model:?} running {benchmark:?} on {runtime:?}"
    )]
    KnobNotAllowed {
        knob: &'static str,
        benchmark: BenchmarkType,
        runtime: RuntimeType,
        model: ModelType,
    },
}

/// Per-cell run-driving knobs, one variant per supported cell. Distinct from
/// `ModelFlags` (generation *identity*): these tune how pipette *drives, gates,
/// and monitors* the run.
///
/// `attempt_timeout_seconds` is the wall-clock limit on each attempt: one
/// warm-up or one measured rep (on `vl_spec_decode`, one sample), each timed
/// on its own. The warm-up is an uncounted attempt with the same limit, and
/// model load and server start count against it. Calibration and
/// readiness-gate waits do not count; each gate has its own
/// `readiness.max_wait_secs`. An attempt past the limit is stopped and the run
/// fails. Unset, the client's default applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, strum::EnumCount)]
#[cfg_attr(test, derive(strum::IntoStaticStr))]
// As on `RuntimeFlags`: the variants, their name tags turned into axis keys.
#[cfg_attr(
    feature = "schemars",
    derive(schemars::JsonSchema),
    schemars(!try_from, !into, tag = "variant", transform = crate::json_schema::triple_arms)
)]
#[serde(try_from = "BenchmarkFlagRef", into = "BenchmarkFlagRef")]
pub enum BenchmarkFlags {
    // llama.cpp CLI timing cells. End-to-end goes through llama-server, so it
    // also bounds the `/completion` call.
    PrefillThroughputLlamacppCliStockToolsGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputLlamacppCliStockToolsGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyLlamacppCliStockToolsGgufText {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // llama.cpp CLI — server cells.
    EvalLlamacppCliStockToolsGgufText {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    VlThroughputLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// Speech recognition through llama-server: the same knobs as
    /// `vl_throughput`, since it is the same request shape and the same gate.
    AsrThroughputLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// Peak-memory VL cell: `http_timeout_seconds` bounds one request,
    /// `attempt_timeout_seconds` the whole attempt. No `readiness`: one
    /// unrepeated, thermal-independent measurement.
    VlMaxMemoryLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    /// Peak-memory cell for speech recognition: the same two timeouts and no
    /// `readiness`, for the reasons the VL cell gives.
    AsrMaxMemoryLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    /// The HTTP timeout plus the readiness gate, as on `vl_throughput`: the
    /// vision tower runs thousands of images back to back.
    VlmEvalLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// As `vlm_eval`: the HTTP timeout plus the readiness gate, since the audio
    /// tower runs thousands of clips back to back.
    AsrEvalLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// A server-driven corpus traversal. `attempt_timeout_seconds` bounds each
    /// sample, all its turns together.
    VlSpecDecodeLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// The same traversal with the drafter on, and the same knobs.
    VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // llama.cpp CLI — peak memory. A bare `llama-bench` subprocess, so the
    // wall-clock deadline is the whole surface.
    MaxMemoryUsageLlamacppCliStockToolsGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    // MLX (macOS).
    PrefillThroughputMlxMacosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputMlxMacosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyMlxMacosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EvalMlxMacosPipetteMlx {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    /// The MLX twin of the llama.cpp CLI `vl_throughput` cell, with its knobs.
    VlThroughputMlxMacosPipetteMlx {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// The MLX twins of the llama.cpp CLI corpus cells, with the same knobs.
    VlSpecDecodeMlxMacosPipetteMlx {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeMlxMacosPipetteMlxSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // iOS (in-process): the attempt limit and `readiness`, with no HTTP client
    // to bound or server to watch. `eval` is absent for that reason; the
    // memory cells take the attempt limit alone, since they do not gate.
    PrefillThroughputLlamacppIosPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// The iOS twins of the corpus cells: `readiness` only.
    VlSpecDecodeLlamacppIosPipetteGgufVision {
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputLlamacppIosPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyLlamacppIosPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlThroughputLlamacppIosPipetteGgufVision {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    AsrThroughputLlamacppIosPipetteGgufVision {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputMlxIosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputMlxIosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyMlxIosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // Android (in-process, as iOS): the attempt limit and `readiness`.
    // `max_memory_usage` does not gate, and the app offers no `eval`.
    PrefillThroughputLlamacppApkPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputLlamacppApkPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyLlamacppApkPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlThroughputLlamacppApkPipetteGgufVision {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // Torch servers, all over HTTP. Throughput cells are one streamed request
    // per rep, so they bound and gate as the latency cell does.
    PrefillThroughputDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EvalDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    EvalUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    VlmEvalUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    AsrEvalUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    EvalDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    EvalUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    /// The SGLang corpus cells, with the same knobs as the llama.cpp and MLX
    /// ones.
    VlSpecDecodeDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeDockerSglangTorchSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeUvSglangTorchSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // OpenVINO (uv venv): a rep is a one-shot driver subprocess, so the attempt
    // limit and `readiness` are the whole surface. `max_memory_usage` does not
    // gate; `eval` and `vl` are unimplemented.
    PrefillThroughputUvOpenvinoOpenvino {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvOpenvinoOpenvino {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvOpenvinoOpenvino {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // OVMS — timing cells. An HTTP server like the torch ones, so the same
    // bound and gate. `eval` and the `vl` cells are unimplemented.
    PrefillThroughputOvmsOpenvino {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputOvmsOpenvino {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyOvmsOpenvino {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // ONNX Runtime GenAI (uv venv): as OpenVINO; `eval` and the `vl` cells are unimplemented.
    PrefillThroughputUvOnnxGenaiOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvOnnxGenaiOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvOnnxGenaiOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // ONNX Runtime GenAI (iOS, in-process): as the other iOS cells. No `eval`:
    // `OGATokenizer` applies no chat template, so the app cannot produce it.
    PrefillThroughputOnnxGenaiIosPipetteOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputOnnxGenaiIosPipetteOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // TensorRT Edge-LLM: the llama.cpp CLI VL cells' knobs. The engine build
    // runs before the server starts, so neither timeout covers it.
    VlThroughputTrtEdgellmGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlMaxMemoryTrtEdgellmGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    // The attempt limit alone: memory cells, which do not gate.
    MaxMemoryUsageMlxMacosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageDockerVllmTorch {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageUvVllmTorch {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageDockerSglangTorch {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageUvSglangTorch {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageUvOpenvinoOpenvino {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageOvmsOpenvino {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageUvOnnxGenaiOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageLlamacppIosPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageMlxIosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageOnnxGenaiIosPipetteOnnxGenai {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageLlamacppApkPipetteGgufText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    MaxMemoryUsageAppleCoreAiMacosPipetteAppleCoreAi {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    VlMaxMemoryMlxMacosPipetteMlx {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    VlMaxMemoryLlamacppIosPipetteGgufVision {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    AsrMaxMemoryLlamacppIosPipetteGgufVision {
        attempt_timeout_seconds: Option<PositiveSeconds>,
    },
    // Core AI and Apple Foundation timing cells: no HTTP server, so the attempt
    // limit and `readiness`.
    PrefillThroughputAppleCoreAiMacosPipetteAppleCoreAi {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputAppleCoreAiMacosPipetteAppleCoreAi {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyAppleCoreAiMacosPipetteAppleCoreAi {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputAppleFoundationAppleFoundationText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyAppleFoundationAppleFoundationText {
        attempt_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
}

impl BenchmarkFlags {
    /// The `(benchmark, runtime, model)` triple this variant encodes.
    pub fn axes(&self) -> (BenchmarkType, RuntimeType, ModelType) {
        use BenchmarkType as B;
        use ModelType as M;
        use RuntimeType as R;
        match self {
            BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. } => {
                (B::PrefillThroughput, R::LlamacppCliStockTools, M::GgufText)
            }
            BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. } => {
                (B::DecodeThroughput, R::LlamacppCliStockTools, M::GgufText)
            }
            BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. } => {
                (B::EndToEndLatency, R::LlamacppCliStockTools, M::GgufText)
            }
            BenchmarkFlags::EvalLlamacppCliStockToolsGgufText { .. } => {
                (B::Eval, R::LlamacppCliStockTools, M::GgufText)
            }
            BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlThroughput, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. } => {
                (B::AsrThroughput, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlMaxMemory, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. } => {
                (B::AsrMaxMemory, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlmEval, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. } => {
                (B::AsrEval, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. } => {
                (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVision)
            }
            BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. } => (
                B::VlSpecDecode,
                R::LlamacppCliStockTools,
                M::GgufVisionSpeculative,
            ),
            BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. } => {
                (B::MaxMemoryUsage, R::LlamacppCliStockTools, M::GgufText)
            }
            BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx { .. } => {
                (B::PrefillThroughput, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx { .. } => {
                (B::DecodeThroughput, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx { .. } => {
                (B::EndToEndLatency, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::EvalMlxMacosPipetteMlx { .. } => (B::Eval, R::MlxMacosPipette, M::Mlx),
            BenchmarkFlags::VlThroughputMlxMacosPipetteMlx { .. } => {
                (B::VlThroughput, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlx { .. } => {
                (B::VlSpecDecode, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. } => {
                (B::VlSpecDecode, R::MlxMacosPipette, M::MlxSpeculative)
            }
            BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. } => {
                (B::PrefillThroughput, R::LlamacppIosPipette, M::GgufText)
            }
            BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. } => {
                (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVision)
            }
            BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. } => (
                B::VlSpecDecode,
                R::LlamacppIosPipette,
                M::GgufVisionSpeculative,
            ),
            BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. } => {
                (B::DecodeThroughput, R::LlamacppIosPipette, M::GgufText)
            }
            BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. } => {
                (B::EndToEndLatency, R::LlamacppIosPipette, M::GgufText)
            }
            BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision { .. } => {
                (B::VlThroughput, R::LlamacppIosPipette, M::GgufVision)
            }
            BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. } => {
                (B::AsrThroughput, R::LlamacppIosPipette, M::GgufVision)
            }
            BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx { .. } => {
                (B::PrefillThroughput, R::MlxIosPipette, M::Mlx)
            }
            BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx { .. } => {
                (B::DecodeThroughput, R::MlxIosPipette, M::Mlx)
            }
            BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx { .. } => {
                (B::EndToEndLatency, R::MlxIosPipette, M::Mlx)
            }
            BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. } => {
                (B::PrefillThroughput, R::LlamacppApkPipette, M::GgufText)
            }
            BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. } => {
                (B::DecodeThroughput, R::LlamacppApkPipette, M::GgufText)
            }
            BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. } => {
                (B::EndToEndLatency, R::LlamacppApkPipette, M::GgufText)
            }
            BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision { .. } => {
                (B::VlThroughput, R::LlamacppApkPipette, M::GgufVision)
            }
            BenchmarkFlags::PrefillThroughputDockerVllmTorch { .. } => {
                (B::PrefillThroughput, R::DockerVllm, M::Torch)
            }
            BenchmarkFlags::DecodeThroughputDockerVllmTorch { .. } => {
                (B::DecodeThroughput, R::DockerVllm, M::Torch)
            }
            BenchmarkFlags::PrefillThroughputUvVllmTorch { .. } => {
                (B::PrefillThroughput, R::UvVllm, M::Torch)
            }
            BenchmarkFlags::DecodeThroughputUvVllmTorch { .. } => {
                (B::DecodeThroughput, R::UvVllm, M::Torch)
            }
            BenchmarkFlags::PrefillThroughputDockerSglangTorch { .. } => {
                (B::PrefillThroughput, R::DockerSglang, M::Torch)
            }
            BenchmarkFlags::DecodeThroughputDockerSglangTorch { .. } => {
                (B::DecodeThroughput, R::DockerSglang, M::Torch)
            }
            BenchmarkFlags::PrefillThroughputUvSglangTorch { .. } => {
                (B::PrefillThroughput, R::UvSglang, M::Torch)
            }
            BenchmarkFlags::DecodeThroughputUvSglangTorch { .. } => {
                (B::DecodeThroughput, R::UvSglang, M::Torch)
            }
            BenchmarkFlags::EndToEndLatencyDockerVllmTorch { .. } => {
                (B::EndToEndLatency, R::DockerVllm, M::Torch)
            }
            BenchmarkFlags::EndToEndLatencyUvVllmTorch { .. } => {
                (B::EndToEndLatency, R::UvVllm, M::Torch)
            }
            BenchmarkFlags::EndToEndLatencyDockerSglangTorch { .. } => {
                (B::EndToEndLatency, R::DockerSglang, M::Torch)
            }
            BenchmarkFlags::EndToEndLatencyUvSglangTorch { .. } => {
                (B::EndToEndLatency, R::UvSglang, M::Torch)
            }
            BenchmarkFlags::EvalDockerVllmTorch { .. } => (B::Eval, R::DockerVllm, M::Torch),
            BenchmarkFlags::EvalUvVllmTorch { .. } => (B::Eval, R::UvVllm, M::Torch),
            BenchmarkFlags::VlmEvalUvVllmTorch { .. } => (B::VlmEval, R::UvVllm, M::Torch),
            BenchmarkFlags::AsrEvalUvVllmTorch { .. } => (B::AsrEval, R::UvVllm, M::Torch),
            BenchmarkFlags::EvalDockerSglangTorch { .. } => (B::Eval, R::DockerSglang, M::Torch),
            BenchmarkFlags::EvalUvSglangTorch { .. } => (B::Eval, R::UvSglang, M::Torch),
            BenchmarkFlags::VlSpecDecodeDockerSglangTorch { .. } => {
                (B::VlSpecDecode, R::DockerSglang, M::Torch)
            }
            BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative { .. } => {
                (B::VlSpecDecode, R::DockerSglang, M::TorchSpeculative)
            }
            BenchmarkFlags::VlSpecDecodeUvSglangTorch { .. } => {
                (B::VlSpecDecode, R::UvSglang, M::Torch)
            }
            BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative { .. } => {
                (B::VlSpecDecode, R::UvSglang, M::TorchSpeculative)
            }
            BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino { .. } => {
                (B::PrefillThroughput, R::UvOpenvino, M::Openvino)
            }
            BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino { .. } => {
                (B::DecodeThroughput, R::UvOpenvino, M::Openvino)
            }
            BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino { .. } => {
                (B::EndToEndLatency, R::UvOpenvino, M::Openvino)
            }
            BenchmarkFlags::PrefillThroughputOvmsOpenvino { .. } => {
                (B::PrefillThroughput, R::Ovms, M::Openvino)
            }
            BenchmarkFlags::DecodeThroughputOvmsOpenvino { .. } => {
                (B::DecodeThroughput, R::Ovms, M::Openvino)
            }
            BenchmarkFlags::EndToEndLatencyOvmsOpenvino { .. } => {
                (B::EndToEndLatency, R::Ovms, M::Openvino)
            }
            BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai { .. } => {
                (B::PrefillThroughput, R::UvOnnxGenai, M::OnnxGenai)
            }
            BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai { .. } => {
                (B::DecodeThroughput, R::UvOnnxGenai, M::OnnxGenai)
            }
            BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai { .. } => {
                (B::EndToEndLatency, R::UvOnnxGenai, M::OnnxGenai)
            }
            BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai { .. } => {
                (B::PrefillThroughput, R::OnnxGenaiIosPipette, M::OnnxGenai)
            }
            BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai { .. } => {
                (B::DecodeThroughput, R::OnnxGenaiIosPipette, M::OnnxGenai)
            }
            BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai { .. } => {
                (B::EndToEndLatency, R::OnnxGenaiIosPipette, M::OnnxGenai)
            }
            BenchmarkFlags::VlThroughputTrtEdgellmGgufVision { .. } => {
                (B::VlThroughput, R::TrtEdgellm, M::GgufVision)
            }
            BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => {
                (B::VlMaxMemory, R::TrtEdgellm, M::GgufVision)
            }
            BenchmarkFlags::MaxMemoryUsageMlxMacosPipetteMlx { .. } => {
                (B::MaxMemoryUsage, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::MaxMemoryUsageDockerVllmTorch { .. } => {
                (B::MaxMemoryUsage, R::DockerVllm, M::Torch)
            }
            BenchmarkFlags::MaxMemoryUsageUvVllmTorch { .. } => {
                (B::MaxMemoryUsage, R::UvVllm, M::Torch)
            }
            BenchmarkFlags::MaxMemoryUsageDockerSglangTorch { .. } => {
                (B::MaxMemoryUsage, R::DockerSglang, M::Torch)
            }
            BenchmarkFlags::MaxMemoryUsageUvSglangTorch { .. } => {
                (B::MaxMemoryUsage, R::UvSglang, M::Torch)
            }
            BenchmarkFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. } => {
                (B::MaxMemoryUsage, R::UvOpenvino, M::Openvino)
            }
            BenchmarkFlags::MaxMemoryUsageOvmsOpenvino { .. } => {
                (B::MaxMemoryUsage, R::Ovms, M::Openvino)
            }
            BenchmarkFlags::MaxMemoryUsageUvOnnxGenaiOnnxGenai { .. } => {
                (B::MaxMemoryUsage, R::UvOnnxGenai, M::OnnxGenai)
            }
            BenchmarkFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. } => {
                (B::MaxMemoryUsage, R::LlamacppIosPipette, M::GgufText)
            }
            BenchmarkFlags::MaxMemoryUsageMlxIosPipetteMlx { .. } => {
                (B::MaxMemoryUsage, R::MlxIosPipette, M::Mlx)
            }
            BenchmarkFlags::MaxMemoryUsageOnnxGenaiIosPipetteOnnxGenai { .. } => {
                (B::MaxMemoryUsage, R::OnnxGenaiIosPipette, M::OnnxGenai)
            }
            BenchmarkFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. } => {
                (B::MaxMemoryUsage, R::LlamacppApkPipette, M::GgufText)
            }
            BenchmarkFlags::MaxMemoryUsageAppleCoreAiMacosPipetteAppleCoreAi { .. } => (
                B::MaxMemoryUsage,
                R::AppleCoreAiMacosPipette,
                M::AppleCoreAi,
            ),
            BenchmarkFlags::VlMaxMemoryMlxMacosPipetteMlx { .. } => {
                (B::VlMaxMemory, R::MlxMacosPipette, M::Mlx)
            }
            BenchmarkFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. } => {
                (B::VlMaxMemory, R::LlamacppIosPipette, M::GgufVision)
            }
            BenchmarkFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. } => {
                (B::AsrMaxMemory, R::LlamacppIosPipette, M::GgufVision)
            }
            BenchmarkFlags::PrefillThroughputAppleCoreAiMacosPipetteAppleCoreAi { .. } => (
                B::PrefillThroughput,
                R::AppleCoreAiMacosPipette,
                M::AppleCoreAi,
            ),
            BenchmarkFlags::DecodeThroughputAppleCoreAiMacosPipetteAppleCoreAi { .. } => (
                B::DecodeThroughput,
                R::AppleCoreAiMacosPipette,
                M::AppleCoreAi,
            ),
            BenchmarkFlags::EndToEndLatencyAppleCoreAiMacosPipetteAppleCoreAi { .. } => (
                B::EndToEndLatency,
                R::AppleCoreAiMacosPipette,
                M::AppleCoreAi,
            ),
            BenchmarkFlags::DecodeThroughputAppleFoundationAppleFoundationText { .. } => (
                B::DecodeThroughput,
                R::AppleFoundation,
                M::AppleFoundationText,
            ),
            BenchmarkFlags::EndToEndLatencyAppleFoundationAppleFoundationText { .. } => (
                B::EndToEndLatency,
                R::AppleFoundation,
                M::AppleFoundationText,
            ),
        }
    }

    /// Whether this entry applies to a cell running `benchmark` on `runtime`
    /// with `model`.
    pub fn matches(&self, benchmark: BenchmarkType, runtime: &Runtime, model: &Model) -> bool {
        self.axes() == (benchmark, RuntimeType::of(runtime), ModelType::of(model))
    }

    /// The HTTP-client timeout that bounds one request to the cell's server, on
    /// the cells that take one.
    pub fn http_timeout(&self) -> Option<PositiveSeconds> {
        match self {
            BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrEvalLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputMlxMacosPipetteMlx {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlx {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputDockerVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputDockerVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputUvVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputUvVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputDockerSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputDockerSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputUvSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputUvSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyDockerVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyUvVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyDockerSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyUvSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EvalLlamacppCliStockToolsGgufText {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EvalMlxMacosPipetteMlx {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EvalDockerVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EvalUvVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlmEvalUvVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrEvalUvVllmTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EvalDockerSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EvalUvSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorch {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputOvmsOpenvino {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputOvmsOpenvino {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyOvmsOpenvino {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputTrtEdgellmGgufVision {
                http_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision {
                http_timeout_seconds,
                ..
            } => *http_timeout_seconds,
            BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::MaxMemoryUsageMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::MaxMemoryUsageDockerVllmTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageUvVllmTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageDockerSglangTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageUvSglangTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | BenchmarkFlags::MaxMemoryUsageUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | BenchmarkFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | BenchmarkFlags::MaxMemoryUsageOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | BenchmarkFlags::MaxMemoryUsageAppleCoreAiMacosPipetteAppleCoreAi { .. }
            | BenchmarkFlags::VlMaxMemoryMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::PrefillThroughputAppleCoreAiMacosPipetteAppleCoreAi { .. }
            | BenchmarkFlags::DecodeThroughputAppleCoreAiMacosPipetteAppleCoreAi { .. }
            | BenchmarkFlags::EndToEndLatencyAppleCoreAiMacosPipetteAppleCoreAi { .. }
            | BenchmarkFlags::DecodeThroughputAppleFoundationAppleFoundationText { .. }
            | BenchmarkFlags::EndToEndLatencyAppleFoundationAppleFoundationText { .. } => None,
        }
    }

    /// The wall-clock limit on one attempt at the cell, on the cells that take
    /// one; `None` keeps the client's default. [`Self::http_timeout`] bounds one
    /// request.
    pub fn attempt_timeout(&self) -> Option<PositiveSeconds> {
        match self {
            BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputMlxMacosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputDockerVllmTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputDockerVllmTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputUvVllmTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputUvVllmTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputDockerSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputDockerSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputUvSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputUvSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyDockerVllmTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyUvVllmTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyDockerSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyUvSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorch {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputOvmsOpenvino {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputOvmsOpenvino {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyOvmsOpenvino {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlThroughputTrtEdgellmGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::MaxMemoryUsageMlxMacosPipetteMlx {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageDockerVllmTorch {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageUvVllmTorch {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageDockerSglangTorch {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageUvSglangTorch {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageUvOpenvinoOpenvino {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageOvmsOpenvino {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageUvOnnxGenaiOnnxGenai {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageLlamacppIosPipetteGgufText {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageMlxIosPipetteMlx {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageOnnxGenaiIosPipetteOnnxGenai {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageLlamacppApkPipetteGgufText {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::MaxMemoryUsageAppleCoreAiMacosPipetteAppleCoreAi {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::VlMaxMemoryMlxMacosPipetteMlx {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::VlMaxMemoryLlamacppIosPipetteGgufVision {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision {
                attempt_timeout_seconds,
            }
            | BenchmarkFlags::PrefillThroughputAppleCoreAiMacosPipetteAppleCoreAi {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputAppleCoreAiMacosPipetteAppleCoreAi {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyAppleCoreAiMacosPipetteAppleCoreAi {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::DecodeThroughputAppleFoundationAppleFoundationText {
                attempt_timeout_seconds,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyAppleFoundationAppleFoundationText {
                attempt_timeout_seconds,
                ..
            } => *attempt_timeout_seconds,
            BenchmarkFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::EvalMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | BenchmarkFlags::EvalDockerVllmTorch { .. }
            | BenchmarkFlags::EvalUvVllmTorch { .. }
            | BenchmarkFlags::VlmEvalUvVllmTorch { .. }
            | BenchmarkFlags::AsrEvalUvVllmTorch { .. }
            | BenchmarkFlags::EvalDockerSglangTorch { .. }
            | BenchmarkFlags::EvalUvSglangTorch { .. } => None,
        }
    }

    /// The host-readiness gate settings; `None` on cells that do not gate and
    /// when the cell set no override.
    pub fn readiness(&self) -> Option<&ReadinessOverrides> {
        match self {
            BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText {
                readiness, ..
            }
            | BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { readiness, .. }
            | BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision { readiness, .. }
            | BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision { readiness, .. }
            | BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision { readiness, .. }
            | BenchmarkFlags::AsrEvalLlamacppCliStockToolsGgufVision { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
                readiness,
                ..
            }
            | BenchmarkFlags::VlThroughputMlxMacosPipetteMlx { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlx { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorch { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorch { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative { readiness, .. }
            | BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx { readiness, .. }
            | BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                readiness,
                ..
            }
            | BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText { readiness, .. }
            | BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision { readiness, .. }
            | BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision { readiness, .. }
            | BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText { readiness, .. }
            | BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx { readiness, .. }
            | BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx { readiness, .. }
            | BenchmarkFlags::PrefillThroughputDockerVllmTorch { readiness, .. }
            | BenchmarkFlags::DecodeThroughputDockerVllmTorch { readiness, .. }
            | BenchmarkFlags::PrefillThroughputUvVllmTorch { readiness, .. }
            | BenchmarkFlags::DecodeThroughputUvVllmTorch { readiness, .. }
            | BenchmarkFlags::PrefillThroughputDockerSglangTorch { readiness, .. }
            | BenchmarkFlags::DecodeThroughputDockerSglangTorch { readiness, .. }
            | BenchmarkFlags::PrefillThroughputUvSglangTorch { readiness, .. }
            | BenchmarkFlags::DecodeThroughputUvSglangTorch { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyDockerVllmTorch { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyUvVllmTorch { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyDockerSglangTorch { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyUvSglangTorch { readiness, .. }
            | BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino { readiness, .. }
            | BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino { readiness, .. }
            | BenchmarkFlags::PrefillThroughputOvmsOpenvino { readiness, .. }
            | BenchmarkFlags::DecodeThroughputOvmsOpenvino { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyOvmsOpenvino { readiness, .. }
            | BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai { readiness, .. }
            | BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai { readiness, .. }
            | BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai { readiness, .. }
            | BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai { readiness, .. }
            | BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText { readiness, .. }
            | BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText { readiness, .. }
            | BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision { readiness, .. }
            | BenchmarkFlags::VlThroughputTrtEdgellmGgufVision { readiness, .. }
            | BenchmarkFlags::PrefillThroughputAppleCoreAiMacosPipetteAppleCoreAi {
                readiness,
                ..
            }
            | BenchmarkFlags::DecodeThroughputAppleCoreAiMacosPipetteAppleCoreAi {
                readiness,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyAppleCoreAiMacosPipetteAppleCoreAi {
                readiness, ..
            }
            | BenchmarkFlags::DecodeThroughputAppleFoundationAppleFoundationText {
                readiness,
                ..
            }
            | BenchmarkFlags::EndToEndLatencyAppleFoundationAppleFoundationText {
                readiness, ..
            } => readiness.as_ref(),
            BenchmarkFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision { .. }
            | BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::EvalMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::EvalDockerVllmTorch { .. }
            | BenchmarkFlags::EvalUvVllmTorch { .. }
            | BenchmarkFlags::VlmEvalUvVllmTorch { .. }
            | BenchmarkFlags::AsrEvalUvVllmTorch { .. }
            | BenchmarkFlags::EvalDockerSglangTorch { .. }
            | BenchmarkFlags::EvalUvSglangTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::MaxMemoryUsageMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::MaxMemoryUsageDockerVllmTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageUvVllmTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageDockerSglangTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageUvSglangTorch { .. }
            | BenchmarkFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | BenchmarkFlags::MaxMemoryUsageUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | BenchmarkFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | BenchmarkFlags::MaxMemoryUsageOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | BenchmarkFlags::MaxMemoryUsageAppleCoreAiMacosPipetteAppleCoreAi { .. }
            | BenchmarkFlags::VlMaxMemoryMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | BenchmarkFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. } => None,
        }
    }
}

/// Flat wire form of [`BenchmarkFlags`]: the three axis keys plus the knobs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(all(test, feature = "schemars"), derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BenchmarkFlagRef {
    pub runtime_type: RuntimeType,
    pub model_type: ModelType,
    pub benchmark_type: BenchmarkType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_timeout_seconds: Option<PositiveSeconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_timeout_seconds: Option<PositiveSeconds>,
    /// Obsolete, absorbed unread: queued specs and recorded results still
    /// carry it, and `deny_unknown_fields` would refuse the cell over it.
    #[serde(rename = "bench_timeout_seconds", default, skip_serializing)]
    pub _bench_timeout_obsolete: Option<serde_json::Value>,
    /// Obsolete, absorbed unread: mgmt still sends `doomloop` on eval claims,
    /// and `deny_unknown_fields` would refuse the cell over it.
    #[serde(rename = "doomloop", default, skip_serializing)]
    pub _doomloop_obsolete: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ReadinessOverrides>,
}

/// The cell coordinates a [`BenchmarkFlags`] variant is tagged with. Stripped
/// before submission: the result names its cell in its own columns.
const AXIS_KEYS: [&str; 3] = ["runtime_type", "model_type", "benchmark_type"];

impl BenchmarkFlags {
    /// What the cell ran under, as the result records it: this value minus the
    /// axis keys, which the result already carries.
    ///
    /// Report the value the run **resolved to**, not the one the plan authored:
    /// a `readiness` block with `None` fields describes a request, not what
    /// happened.
    pub fn submission_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self)
            // Unreachable: `BenchmarkFlags` serializes infallibly. Fall closed
            // on an empty object rather than dropping the record.
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(fields) = value.as_object_mut() {
            AXIS_KEYS.iter().for_each(|key| {
                fields.remove(*key);
            });
        }
        value
    }
}

/// Why a recorded [`BenchmarkFlags`] value did not read back against its cell.
pub type BenchmarkFlagsRecordError = super::FlagsRecordError<BenchmarkFlagsError>;

impl BenchmarkFlags {
    /// A [`Self::submission_value`] read back against the cell it was recorded
    /// for, as [`RuntimeFlags::from_cell_json`](crate::RuntimeFlags::from_cell_json)
    /// reads a client's knobs. An empty object is `None`.
    pub fn from_cell_json(
        json: &str,
        runtime_type: RuntimeType,
        model_type: ModelType,
        benchmark_type: BenchmarkType,
    ) -> Result<Option<Self>, BenchmarkFlagsRecordError> {
        let axes = [
            ("runtime_type", serde_json::to_value(runtime_type)?),
            ("model_type", serde_json::to_value(model_type)?),
            ("benchmark_type", serde_json::to_value(benchmark_type)?),
        ];
        super::record::with_cell::<BenchmarkFlagRef, _>(json, &axes)?
            .map(Self::try_from)
            .transpose()
            .map_err(BenchmarkFlagsRecordError::Flags)
    }
}

impl BenchmarkFlagRef {
    /// Reject a knob that's set but not accepted by this cell.
    fn deny(&self, set: bool, knob: &'static str) -> Result<(), BenchmarkFlagsError> {
        if set {
            Err(BenchmarkFlagsError::KnobNotAllowed {
                knob,
                benchmark: self.benchmark_type,
                runtime: self.runtime_type,
                model: self.model_type,
            })
        } else {
            Ok(())
        }
    }

    fn deny_http(&self) -> Result<(), BenchmarkFlagsError> {
        self.deny(self.http_timeout_seconds.is_some(), "http_timeout_seconds")
    }

    fn deny_attempt_timeout(&self) -> Result<(), BenchmarkFlagsError> {
        self.deny(
            self.attempt_timeout_seconds.is_some(),
            "attempt_timeout_seconds",
        )
    }

    fn deny_readiness(&self) -> Result<(), BenchmarkFlagsError> {
        self.deny(self.readiness.is_some(), "readiness")
    }
}

impl TryFrom<BenchmarkFlagRef> for BenchmarkFlags {
    type Error = BenchmarkFlagsError;

    fn try_from(r: BenchmarkFlagRef) -> Result<Self, Self::Error> {
        use BenchmarkType as B;
        use ModelType as M;
        use RuntimeType as R;

        let flags = match (r.benchmark_type, r.runtime_type, r.model_type) {
            // The corpus cells: every knob applies, on either engine and with
            // the drafter on or off.
            (B::VlSpecDecode, R::MlxMacosPipette, M::Mlx) => {
                BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlx {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::MlxMacosPipette, M::MlxSpeculative) => {
                BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::DockerSglang, M::Torch) => {
                BenchmarkFlags::VlSpecDecodeDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::DockerSglang, M::TorchSpeculative) => {
                BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::UvSglang, M::Torch) => BenchmarkFlags::VlSpecDecodeUvSglangTorch {
                http_timeout_seconds: r.http_timeout_seconds,
                attempt_timeout_seconds: r.attempt_timeout_seconds,
                readiness: r.readiness,
            },
            (B::VlSpecDecode, R::UvSglang, M::TorchSpeculative) => {
                BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVision) => {
                BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVisionSpeculative) => {
                BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // Timing cells.
            (B::PrefillThroughput, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::LlamacppCliStockTools, M::GgufText) => {
                BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // The iOS corpus cells: the gate, and nothing that bounds a
            // process this engine does not spawn.
            (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision {
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVisionSpeculative) => {
                r.deny_http()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlThroughput, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::AsrThroughput, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlThroughput, R::LlamacppApkPipette, M::GgufVision) => {
                r.deny_http()?;
                BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::Ovms, M::Openvino) => {
                BenchmarkFlags::PrefillThroughputOvmsOpenvino {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::Ovms, M::Openvino) => {
                BenchmarkFlags::DecodeThroughputOvmsOpenvino {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::Ovms, M::Openvino) => {
                BenchmarkFlags::EndToEndLatencyOvmsOpenvino {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::DockerVllm, M::Torch) => {
                BenchmarkFlags::PrefillThroughputDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::DockerVllm, M::Torch) => {
                BenchmarkFlags::DecodeThroughputDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvVllm, M::Torch) => {
                BenchmarkFlags::PrefillThroughputUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvVllm, M::Torch) => {
                BenchmarkFlags::DecodeThroughputUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::DockerSglang, M::Torch) => {
                BenchmarkFlags::PrefillThroughputDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::DockerSglang, M::Torch) => {
                BenchmarkFlags::DecodeThroughputDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvSglang, M::Torch) => {
                BenchmarkFlags::PrefillThroughputUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvSglang, M::Torch) => {
                BenchmarkFlags::DecodeThroughputUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::DockerVllm, M::Torch) => {
                BenchmarkFlags::EndToEndLatencyDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvVllm, M::Torch) => {
                BenchmarkFlags::EndToEndLatencyUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::DockerSglang, M::Torch) => {
                BenchmarkFlags::EndToEndLatencyDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvSglang, M::Torch) => {
                BenchmarkFlags::EndToEndLatencyUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // vl-throughput: HTTP timeout + readiness.
            (B::VlThroughput, R::LlamacppCliStockTools, M::GgufVision) => {
                BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // As vl_throughput.
            (B::AsrThroughput, R::LlamacppCliStockTools, M::GgufVision) => {
                BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlThroughput, R::MlxMacosPipette, M::Mlx) => {
                BenchmarkFlags::VlThroughputMlxMacosPipetteMlx {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // vlm-eval: HTTP timeout + readiness, same shape as vl-throughput —
            // the vision tower runs thousands of images back-to-back, so the
            // readiness gate matters here as much as on the timing cells.
            (B::VlmEval, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_attempt_timeout()?;
                BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // asr-eval: the vlm_eval shape, clips instead of images.
            (B::AsrEval, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_attempt_timeout()?;
                BenchmarkFlags::AsrEvalLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // vl-max-memory: both timeouts (a single ungated run).
            (B::VlMaxMemory, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_readiness()?;
                BenchmarkFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            // As vl_max_memory.
            (B::AsrMaxMemory, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_readiness()?;
                BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            // TensorRT Edge-LLM: as the llama.cpp CLI VL cells above.
            (B::VlThroughput, R::TrtEdgellm, M::GgufVision) => {
                BenchmarkFlags::VlThroughputTrtEdgellmGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlMaxMemory, R::TrtEdgellm, M::GgufVision) => {
                r.deny_readiness()?;
                BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            // max_memory_usage: the subprocess deadline only (see module docs).
            (B::MaxMemoryUsage, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            // eval: HTTP timeout (no readiness).
            (B::Eval, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::EvalLlamacppCliStockToolsGgufText {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::MlxMacosPipette, M::Mlx) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::EvalMlxMacosPipetteMlx {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::DockerVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::EvalDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::UvVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::EvalUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::VlmEval, R::UvVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::VlmEvalUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::AsrEval, R::UvVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::AsrEvalUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::DockerSglang, M::Torch) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::EvalDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::UvSglang, M::Torch) => {
                r.deny_readiness()?;
                r.deny_attempt_timeout()?;
                BenchmarkFlags::EvalUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageMlxMacosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::DockerVllm, M::Torch) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageDockerVllmTorch {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::UvVllm, M::Torch) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageUvVllmTorch {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::DockerSglang, M::Torch) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageDockerSglangTorch {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::UvSglang, M::Torch) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageUvSglangTorch {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageUvOpenvinoOpenvino {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::Ovms, M::Openvino) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageOvmsOpenvino {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageUvOnnxGenaiOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageLlamacppIosPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageMlxIosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageOnnxGenaiIosPipetteOnnxGenai {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageLlamacppApkPipetteGgufText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::MaxMemoryUsage, R::AppleCoreAiMacosPipette, M::AppleCoreAi) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageAppleCoreAiMacosPipetteAppleCoreAi {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::VlMaxMemory, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::VlMaxMemoryMlxMacosPipetteMlx {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::VlMaxMemory, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::VlMaxMemoryLlamacppIosPipetteGgufVision {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::AsrMaxMemory, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                }
            }
            (B::PrefillThroughput, R::AppleCoreAiMacosPipette, M::AppleCoreAi) => {
                r.deny_http()?;
                BenchmarkFlags::PrefillThroughputAppleCoreAiMacosPipetteAppleCoreAi {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::AppleCoreAiMacosPipette, M::AppleCoreAi) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputAppleCoreAiMacosPipetteAppleCoreAi {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::AppleCoreAiMacosPipette, M::AppleCoreAi) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyAppleCoreAiMacosPipetteAppleCoreAi {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::AppleFoundation, M::AppleFoundationText) => {
                r.deny_http()?;
                BenchmarkFlags::DecodeThroughputAppleFoundationAppleFoundationText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::AppleFoundation, M::AppleFoundationText) => {
                r.deny_http()?;
                BenchmarkFlags::EndToEndLatencyAppleFoundationAppleFoundationText {
                    attempt_timeout_seconds: r.attempt_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            _ => {
                return Err(BenchmarkFlagsError::NoSuchCombination {
                    benchmark: r.benchmark_type,
                    runtime: r.runtime_type,
                    model: r.model_type,
                });
            }
        };
        Ok(flags)
    }
}

impl From<BenchmarkFlags> for BenchmarkFlagRef {
    fn from(f: BenchmarkFlags) -> Self {
        let (benchmark_type, runtime_type, model_type) = f.axes();
        BenchmarkFlagRef {
            runtime_type,
            model_type,
            benchmark_type,
            http_timeout_seconds: f.http_timeout(),
            attempt_timeout_seconds: f.attempt_timeout(),
            _doomloop_obsolete: None,
            _bench_timeout_obsolete: None,
            readiness: f.readiness().cloned(),
        }
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;

    fn prefill_flags(readiness: Option<ReadinessOverrides>) -> BenchmarkFlags {
        BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText {
            attempt_timeout_seconds: None,
            readiness,
        }
    }

    #[test]
    fn the_cell_coordinates_are_not_reported() {
        let value = prefill_flags(None).submission_value();

        AXIS_KEYS.iter().for_each(|key| {
            assert_eq!(value.get(*key), None, "{key} should not be reported");
        });
    }

    #[test]
    fn a_recorded_value_reads_back_against_its_cell() -> anyhow::Result<()> {
        let flags = prefill_flags(Some(ReadinessOverrides {
            max_wait_secs: Some(300),
            skip_thermal: Some(false),
            skin_below_c: Some(SkinCelsius::try_new(34)?),
        }));
        let read = BenchmarkFlags::from_cell_json(
            &flags.submission_value().to_string(),
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
            BenchmarkType::PrefillThroughput,
        )?;
        assert_eq!(read, Some(flags));
        Ok(())
    }

    #[test]
    fn a_resolved_readiness_block_reports_skin_even_when_thermal_is_skipped() -> anyhow::Result<()>
    {
        let flags = prefill_flags(Some(ReadinessOverrides {
            max_wait_secs: Some(300),
            skip_thermal: Some(true),
            skin_below_c: Some(SkinCelsius::try_new(34)?),
        }));

        assert_eq!(
            flags.submission_value().get("readiness"),
            Some(
                &serde_json::json!({"max_wait_secs": 300, "skip_thermal": true, "skin_below_c": 34})
            )
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case(0, false)]
    #[case(1, true)]
    #[case(110, true)]
    #[case(111, false)]
    fn skin_threshold_range_is_checked(#[case] value: u32, #[case] accepted: bool) {
        let json = format!(r#"{{"skin_below_c":{value}}}"#);
        assert_eq!(
            serde_json::from_str::<ReadinessOverrides>(&json).is_ok(),
            accepted
        );
    }

    /// Eval never gates, so a readiness block would record a policy the run did
    /// not apply.
    #[test]
    fn an_ungated_cell_refuses_a_readiness_block() {
        let cell = BenchmarkFlagRef {
            benchmark_type: BenchmarkType::Eval,
            runtime_type: RuntimeType::LlamacppCliStockTools,
            model_type: ModelType::GgufText,
            http_timeout_seconds: None,
            attempt_timeout_seconds: None,
            _doomloop_obsolete: None,
            _bench_timeout_obsolete: None,
            readiness: Some(ReadinessOverrides {
                max_wait_secs: Some(300),
                skip_thermal: Some(false),
                skin_below_c: None,
            }),
        };

        assert!(matches!(
            BenchmarkFlags::try_from(cell),
            Err(BenchmarkFlagsError::KnobNotAllowed {
                knob: "readiness",
                ..
            })
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_ref(
        benchmark_type: BenchmarkType,
        runtime_type: RuntimeType,
        model_type: ModelType,
    ) -> BenchmarkFlagRef {
        BenchmarkFlagRef {
            runtime_type,
            model_type,
            benchmark_type,
            http_timeout_seconds: None,
            attempt_timeout_seconds: None,
            _doomloop_obsolete: None,
            _bench_timeout_obsolete: None,
            readiness: None,
        }
    }

    #[test]
    fn eval_carries_http_no_readiness() -> anyhow::Result<()> {
        let mut r = cell_ref(
            BenchmarkType::Eval,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
        );
        r.http_timeout_seconds = Some(PositiveSeconds::try_new(600)?);
        let flags = BenchmarkFlags::try_from(r)?;
        assert_eq!(flags.http_timeout(), Some(PositiveSeconds::try_new(600)?));
        assert!(flags.readiness().is_none());
        assert_eq!(
            flags.axes(),
            (
                BenchmarkType::Eval,
                RuntimeType::LlamacppCliStockTools,
                ModelType::GgufText
            )
        );
        Ok(())
    }

    /// A readiness override, `skip_thermal` included (the [`CELLS`] sweep does
    /// not probe it), survives `TryFrom` and the flat-wire round trip.
    #[rstest::rstest]
    #[case::mlx_prefill(
        BenchmarkType::PrefillThroughput,
        RuntimeType::MlxMacosPipette,
        ModelType::Mlx,
        "mlx_macos_pipette"
    )]
    #[case::torch_end_to_end(
        BenchmarkType::EndToEndLatency,
        RuntimeType::DockerVllm,
        ModelType::Torch,
        "docker_vllm"
    )]
    #[case::openvino_prefill(
        BenchmarkType::PrefillThroughput,
        RuntimeType::UvOpenvino,
        ModelType::Openvino,
        "uv_openvino"
    )]
    #[case::openvino_decode(
        BenchmarkType::DecodeThroughput,
        RuntimeType::UvOpenvino,
        ModelType::Openvino,
        "uv_openvino"
    )]
    #[case::openvino_end_to_end(
        BenchmarkType::EndToEndLatency,
        RuntimeType::UvOpenvino,
        ModelType::Openvino,
        "uv_openvino"
    )]
    #[case::ovms_prefill(
        BenchmarkType::PrefillThroughput,
        RuntimeType::Ovms,
        ModelType::Openvino,
        "ovms"
    )]
    #[case::ovms_decode(
        BenchmarkType::DecodeThroughput,
        RuntimeType::Ovms,
        ModelType::Openvino,
        "ovms"
    )]
    #[case::ovms_end_to_end(
        BenchmarkType::EndToEndLatency,
        RuntimeType::Ovms,
        ModelType::Openvino,
        "ovms"
    )]
    #[case::onnx_genai_prefill(
        BenchmarkType::PrefillThroughput,
        RuntimeType::UvOnnxGenai,
        ModelType::OnnxGenai,
        "uv_onnx_genai"
    )]
    #[case::onnx_genai_decode(
        BenchmarkType::DecodeThroughput,
        RuntimeType::UvOnnxGenai,
        ModelType::OnnxGenai,
        "uv_onnx_genai"
    )]
    #[case::onnx_genai_end_to_end(
        BenchmarkType::EndToEndLatency,
        RuntimeType::UvOnnxGenai,
        ModelType::OnnxGenai,
        "uv_onnx_genai"
    )]
    #[case::onnx_genai_ios_prefill(
        BenchmarkType::PrefillThroughput,
        RuntimeType::OnnxGenaiIosPipette,
        ModelType::OnnxGenai,
        "onnx_genai_ios_pipette"
    )]
    #[case::onnx_genai_ios_decode(
        BenchmarkType::DecodeThroughput,
        RuntimeType::OnnxGenaiIosPipette,
        ModelType::OnnxGenai,
        "onnx_genai_ios_pipette"
    )]
    #[case::onnx_genai_ios_end_to_end(
        BenchmarkType::EndToEndLatency,
        RuntimeType::OnnxGenaiIosPipette,
        ModelType::OnnxGenai,
        "onnx_genai_ios_pipette"
    )]
    fn readiness_on_a_timing_cell_round_trips(
        #[case] benchmark: BenchmarkType,
        #[case] runtime: RuntimeType,
        #[case] model: ModelType,
        #[case] runtime_wire: &str,
    ) -> anyhow::Result<()> {
        let mut r = cell_ref(benchmark, runtime, model);
        r.readiness = Some(ReadinessOverrides {
            skip_thermal: Some(true),
            max_wait_secs: Some(1800),
            skin_below_c: Some(SkinCelsius::try_new(34)?),
        });
        let flags = BenchmarkFlags::try_from(r)?;
        assert_eq!(flags.readiness().and_then(|x| x.max_wait_secs), Some(1800));
        assert_eq!(flags.readiness().and_then(|x| x.skip_thermal), Some(true));
        assert_eq!(
            flags
                .readiness()
                .and_then(|x| x.skin_below_c)
                .map(SkinCelsius::into_inner),
            Some(34)
        );

        let wire = toml::to_string(&flags)?;
        assert!(
            wire.contains(&format!(r#"runtime_type = "{runtime_wire}""#)),
            "got:\n{wire}"
        );
        assert!(wire.contains("max_wait_secs = 1800"), "got:\n{wire}");
        let round: BenchmarkFlags = toml::from_str(&wire)?;
        assert_eq!(flags, round);
        Ok(())
    }

    const LLAMACPP_CLI: &str = "type = \"llamacpp_cli_stock_tools\"\nsource = \"github_release\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b5000\"\nflavor = \"macos-arm64\"";
    const LLAMACPP_IOS: &str = "type = \"llamacpp_ios_pipette\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b9050\"\nflavor = \"ios-arm64\"";
    const GGUF_TEXT: &str = "type = \"gguf_text\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\npath = \"m.gguf\"";
    const GGUF_VISION: &str = "type = \"gguf_vision\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\nmodel = \"m.gguf\"\nmmproj = \"mm.gguf\"";

    /// Each miss differs from the entry's cell on one axis alone.
    #[rstest::rstest]
    #[case::its_cell(BenchmarkType::EndToEndLatency, LLAMACPP_CLI, GGUF_TEXT, true)]
    #[case::another_benchmark(BenchmarkType::Eval, LLAMACPP_CLI, GGUF_TEXT, false)]
    #[case::another_runtime(BenchmarkType::EndToEndLatency, LLAMACPP_IOS, GGUF_TEXT, false)]
    #[case::another_model(BenchmarkType::EndToEndLatency, LLAMACPP_CLI, GGUF_VISION, false)]
    fn benchmark_flags_match_only_their_cell(
        #[case] benchmark: BenchmarkType,
        #[case] runtime: &str,
        #[case] model: &str,
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        let flags = BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText {
            http_timeout_seconds: None,
            attempt_timeout_seconds: None,
            readiness: None,
        };
        let (runtime, model): (Runtime, Model) = (toml::from_str(runtime)?, toml::from_str(model)?);
        assert_eq!(flags.matches(benchmark, &runtime, &model), expected);
        Ok(())
    }

    #[test]
    fn readiness_on_eval_is_rejected() {
        let mut r = cell_ref(
            BenchmarkType::Eval,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
        );
        r.readiness = Some(ReadinessOverrides {
            skip_thermal: None,
            max_wait_secs: Some(60),
            skin_below_c: None,
        });
        assert!(matches!(
            BenchmarkFlags::try_from(r),
            Err(BenchmarkFlagsError::KnobNotAllowed {
                knob: "readiness",
                ..
            })
        ));
    }

    #[test]
    fn http_timeout_on_timing_cell_is_rejected() -> anyhow::Result<()> {
        let mut r = cell_ref(
            BenchmarkType::DecodeThroughput,
            RuntimeType::MlxMacosPipette,
            ModelType::Mlx,
        );
        r.http_timeout_seconds = Some(PositiveSeconds::try_new(600)?);
        assert!(matches!(
            BenchmarkFlags::try_from(r),
            Err(BenchmarkFlagsError::KnobNotAllowed {
                knob: "http_timeout_seconds",
                ..
            })
        ));
        Ok(())
    }

    /// `readiness` is rejected on the single ungated run, not silently ignored.
    #[test]
    fn vl_max_memory_takes_both_timeouts_but_not_readiness() -> anyhow::Result<()> {
        let mut r = cell_ref(
            BenchmarkType::VlMaxMemory,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufVision,
        );
        r.http_timeout_seconds = Some(PositiveSeconds::try_new(900)?);
        r.attempt_timeout_seconds = Some(PositiveSeconds::try_new(2400)?);
        let flags = BenchmarkFlags::try_from(r)?;
        assert_eq!(flags.http_timeout(), Some(PositiveSeconds::try_new(900)?));
        assert_eq!(
            flags.attempt_timeout(),
            Some(PositiveSeconds::try_new(2400)?)
        );
        assert!(flags.readiness().is_none());

        let mut r = cell_ref(
            BenchmarkType::VlMaxMemory,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufVision,
        );
        r.readiness = Some(ReadinessOverrides::default());
        assert!(matches!(
            BenchmarkFlags::try_from(r),
            Err(BenchmarkFlagsError::KnobNotAllowed {
                knob: "readiness",
                ..
            })
        ));
        Ok(())
    }

    #[rstest::rstest]
    #[case::eval("eval")]
    #[case::timing("prefill_throughput")]
    fn a_doomloop_key_is_absorbed_and_dropped(#[case] benchmark: &str) -> anyhow::Result<()> {
        let wire = format!(
            "benchmark_type = \"{benchmark}\"\n\
             runtime_type = \"llamacpp_cli_stock_tools\"\n\
             model_type = \"gguf_text\"\n\
             doomloop = {{ exact_repeat = {{ required = 5, window = 8192 }} }}\n"
        );
        let flags = toml::from_str::<BenchmarkFlags>(&wire)?;
        assert!(!toml::to_string(&flags)?.contains("doomloop"));
        assert!(!flags.submission_value().to_string().contains("doomloop"));
        Ok(())
    }

    #[test]
    fn unsupported_cells_have_no_variant() {
        for r in [
            cell_ref(
                BenchmarkType::Eval,
                RuntimeType::LlamacppCliStockTools,
                ModelType::AppleFoundationText,
            ),
            cell_ref(
                BenchmarkType::Eval,
                RuntimeType::LlamacppApkPipette,
                ModelType::GgufText,
            ),
            cell_ref(
                BenchmarkType::Eval,
                RuntimeType::LlamacppIosPipette,
                ModelType::GgufText,
            ),
            // Apple Foundation runs no prefill cell.
            cell_ref(
                BenchmarkType::PrefillThroughput,
                RuntimeType::AppleFoundation,
                ModelType::AppleFoundationText,
            ),
            // Only the two VL cells run on TensorRT Edge-LLM.
            cell_ref(
                BenchmarkType::PrefillThroughput,
                RuntimeType::TrtEdgellm,
                ModelType::GgufVision,
            ),
            cell_ref(
                BenchmarkType::VlmEval,
                RuntimeType::TrtEdgellm,
                ModelType::GgufVision,
            ),
        ] {
            assert!(matches!(
                BenchmarkFlags::try_from(r),
                Err(BenchmarkFlagsError::NoSuchCombination { .. })
            ));
        }
    }

    /// A plan bypasses clap's `range(1..)`, so the type itself refuses zero.
    #[rstest::rstest]
    #[case::text_bench("max_memory_usage", "gguf_text", "attempt_timeout_seconds")]
    #[case::vl_bench("vl_max_memory", "gguf_vision", "attempt_timeout_seconds")]
    #[case::vl_http("vl_max_memory", "gguf_vision", "http_timeout_seconds")]
    #[case::eval_http("eval", "gguf_text", "http_timeout_seconds")]
    fn a_zero_timeout_is_rejected(
        #[case] benchmark: &str,
        #[case] model: &str,
        #[case] knob: &str,
    ) {
        let wire = format!(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"{model}\"\nbenchmark_type = \"{benchmark}\"\n{knob} = 0\n"
        );
        let err = parse(&wire)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.contains(knob) && err.contains("greater than 0"),
            "{knob}: {err:?}"
        );
    }

    #[test]
    fn the_old_limit_is_absorbed_and_dropped() -> anyhow::Result<()> {
        let wire = format!(
            "{}bench_timeout_seconds = 1200\nattempt_timeout_seconds = 300\n",
            axes_wire("max_memory_usage", "llamacpp_cli_stock_tools", "gguf_text")
        );
        let flags = parse(&wire)?;
        anyhow::ensure!(flags.attempt_timeout() == Some(PositiveSeconds::try_new(300)?));
        let (toml, json) = (toml::to_string(&flags)?, serde_json::to_string(&flags)?);
        anyhow::ensure!(!toml.contains("bench_timeout"), "toml: {toml}");
        anyhow::ensure!(!json.contains("bench_timeout"), "json: {json}");
        Ok(())
    }

    /// Results recorded before the new field carry the old key.
    #[test]
    fn a_stored_row_with_the_old_limit_reads_back_without_one() -> anyhow::Result<()> {
        let read = BenchmarkFlags::from_cell_json(
            r#"{"http_timeout_seconds":600,"bench_timeout_seconds":10800}"#,
            RuntimeType::MlxMacosPipette,
            ModelType::Mlx,
            BenchmarkType::VlSpecDecode,
        )?;
        anyhow::ensure!(
            read.as_ref().is_some_and(|f| f.attempt_timeout().is_none()),
            "read back as {read:?}"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case("eval", "llamacpp_cli_stock_tools", "gguf_text")]
    #[case("eval", "uv_vllm", "torch")]
    #[case("vlm_eval", "llamacpp_cli_stock_tools", "gguf_vision")]
    #[case("asr_eval", "uv_vllm", "torch")]
    #[case("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision")]
    fn cells_without_an_attempt_limit_refuse_it_by_name(
        #[case] benchmark: &str,
        #[case] runtime: &str,
        #[case] model: &str,
    ) -> anyhow::Result<()> {
        let wire = format!(
            "{}attempt_timeout_seconds = 300\n",
            axes_wire(benchmark, runtime, model)
        );
        let Err(error) = parse(&wire) else {
            anyhow::bail!("{benchmark}/{runtime}/{model} accepted the attempt limit");
        };
        anyhow::ensure!(
            error.to_string().contains("attempt_timeout_seconds"),
            "the refusal does not name the knob: {error}"
        );
        Ok(())
    }

    #[test]
    fn unset_knobs_omitted_from_wire() -> anyhow::Result<()> {
        let flags = BenchmarkFlags::try_from(cell_ref(
            BenchmarkType::Eval,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
        ))?;
        let wire = toml::to_string(&flags)?;
        assert!(!wire.contains("http_timeout_seconds"), "got:\n{wire}");
        assert!(!wire.contains("doomloop"), "got:\n{wire}");
        assert!(!wire.contains("readiness"), "got:\n{wire}");
        Ok(())
    }

    /// Every settable field on [`BenchmarkFlagRef`] but the absorbed
    /// `doomloop`, with a valid wire value.
    const PROBES: &[(&str, &str)] = &[
        ("http_timeout_seconds", "600"),
        ("attempt_timeout_seconds", "1800"),
        ("readiness", "{ max_wait_secs = 120 }"),
    ];

    /// Every cell with a variant and its accepted fields. Transcribed by hand,
    /// not derived, so a variant's field change without an edit here fails.
    #[rustfmt::skip]
    const CELLS: &[(&str, &str, &str, &[&str])] = &[
        ("prefill_throughput", "llamacpp_cli_stock_tools", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "llamacpp_cli_stock_tools", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "llamacpp_cli_stock_tools", "gguf_text",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("asr_throughput", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "mlx_macos_pipette", "mlx", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "mlx_macos_pipette", "mlx", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "mlx_macos_pipette", "mlx", &["attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "llamacpp_ios_pipette", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "llamacpp_ios_pipette", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "llamacpp_ios_pipette", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("vl_throughput", "llamacpp_ios_pipette", "gguf_vision", &["attempt_timeout_seconds", "readiness"]),
        ("asr_throughput", "llamacpp_ios_pipette", "gguf_vision", &["attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision", &["readiness"]),
        ("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision_speculative",
         &["readiness"]),
        ("prefill_throughput", "mlx_ios_pipette", "mlx", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "mlx_ios_pipette", "mlx", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "mlx_ios_pipette", "mlx", &["attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "llamacpp_apk_pipette", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "llamacpp_apk_pipette", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "llamacpp_apk_pipette", "gguf_text", &["attempt_timeout_seconds", "readiness"]),
        ("vl_throughput", "llamacpp_apk_pipette", "gguf_vision", &["attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "docker_vllm", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "docker_vllm", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "docker_vllm", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_vllm", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "uv_vllm", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "uv_vllm", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "docker_sglang", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "docker_sglang", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "docker_sglang", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_sglang", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "uv_sglang", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "uv_sglang", "torch", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_openvino", "openvino", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "uv_openvino", "openvino", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "uv_openvino", "openvino", &["attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "ovms", "openvino", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "ovms", "openvino", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "ovms", "openvino", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_onnx_genai", "onnx_genai", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "uv_onnx_genai", "onnx_genai", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "uv_onnx_genai", "onnx_genai", &["attempt_timeout_seconds", "readiness"]),
        ("prefill_throughput", "onnx_genai_ios_pipette", "onnx_genai", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "onnx_genai_ios_pipette", "onnx_genai", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "onnx_genai_ios_pipette", "onnx_genai", &["attempt_timeout_seconds", "readiness"]),
        ("vl_throughput", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        // vlm-eval: an HTTP server run, same shape as vl_throughput — the
        // vision tower runs thousands of images back-to-back, so the
        // readiness gate matters here as much as on the timing cells.
        ("vlm_eval", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "readiness"]),
        ("asr_eval", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "readiness"]),
        ("vl_throughput", "mlx_macos_pipette", "mlx", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_max_memory", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "attempt_timeout_seconds"]),
        ("vl_throughput", "trt_edgellm", "gguf_vision", &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_max_memory", "trt_edgellm", "gguf_vision",
         &["http_timeout_seconds", "attempt_timeout_seconds"]),
        ("asr_max_memory", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "attempt_timeout_seconds"]),
        ("vl_spec_decode", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "llamacpp_cli_stock_tools", "gguf_vision_speculative",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "mlx_macos_pipette", "mlx",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "mlx_macos_pipette", "mlx_speculative",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("max_memory_usage", "llamacpp_cli_stock_tools", "gguf_text",
         &["attempt_timeout_seconds"]),
        ("eval", "llamacpp_cli_stock_tools", "gguf_text", &["http_timeout_seconds"]),
        ("eval", "mlx_macos_pipette", "mlx", &["http_timeout_seconds"]),
        ("eval", "docker_vllm", "torch", &["http_timeout_seconds"]),
        ("eval", "uv_vllm", "torch", &["http_timeout_seconds"]),
        ("vlm_eval", "uv_vllm", "torch", &["http_timeout_seconds"]),
        ("asr_eval", "uv_vllm", "torch", &["http_timeout_seconds"]),
        ("eval", "docker_sglang", "torch", &["http_timeout_seconds"]),
        ("eval", "uv_sglang", "torch", &["http_timeout_seconds"]),
        ("vl_spec_decode", "docker_sglang", "torch",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "docker_sglang", "torch_speculative",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "uv_sglang", "torch",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "uv_sglang", "torch_speculative",
         &["http_timeout_seconds", "attempt_timeout_seconds", "readiness"]),
        ("max_memory_usage", "mlx_macos_pipette", "mlx", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "docker_vllm", "torch", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "uv_vllm", "torch", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "docker_sglang", "torch", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "uv_sglang", "torch", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "uv_openvino", "openvino", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "ovms", "openvino", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "uv_onnx_genai", "onnx_genai", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "llamacpp_ios_pipette", "gguf_text", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "mlx_ios_pipette", "mlx", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "onnx_genai_ios_pipette", "onnx_genai", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "llamacpp_apk_pipette", "gguf_text", &["attempt_timeout_seconds"]),
        ("max_memory_usage", "core_ai_macos_pipette", "core_ai", &["attempt_timeout_seconds"]),
        ("vl_max_memory", "mlx_macos_pipette", "mlx", &["attempt_timeout_seconds"]),
        ("vl_max_memory", "llamacpp_ios_pipette", "gguf_vision", &["attempt_timeout_seconds"]),
        ("asr_max_memory", "llamacpp_ios_pipette", "gguf_vision", &["attempt_timeout_seconds"]),
        ("prefill_throughput", "core_ai_macos_pipette", "core_ai", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "core_ai_macos_pipette", "core_ai", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "core_ai_macos_pipette", "core_ai", &["attempt_timeout_seconds", "readiness"]),
        ("decode_throughput", "apple_foundation", "apple_foundation_text", &["attempt_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "apple_foundation", "apple_foundation_text", &["attempt_timeout_seconds", "readiness"]),
    ];

    fn axes_wire(benchmark: &str, runtime: &str, model: &str) -> String {
        format!(
            "runtime_type = \"{runtime}\"\nmodel_type = \"{model}\"\nbenchmark_type = \"{benchmark}\"\n"
        )
    }

    fn parse(wire: &str) -> Result<BenchmarkFlags, toml::de::Error> {
        toml::from_str(wire)
    }

    /// A plan still authoring the retired `cooldown_seconds` must fail by name,
    /// not run without the hold it asked for.
    #[rstest::rstest]
    #[case("llamacpp_cli_stock_tools", "gguf_vision_speculative")]
    #[case("llamacpp_ios_pipette", "gguf_vision_speculative")]
    #[case("mlx_macos_pipette", "mlx_speculative")]
    fn the_retired_cooldown_knob_is_refused(
        #[case] runtime: &str,
        #[case] model: &str,
    ) -> anyhow::Result<()> {
        let wire = format!(
            "{}cooldown_seconds = 240\n",
            axes_wire("vl_spec_decode", runtime, model)
        );
        let Err(error) = parse(&wire) else {
            anyhow::bail!("{runtime} accepted `cooldown_seconds`");
        };
        anyhow::ensure!(
            error.to_string().contains("cooldown_seconds"),
            "the refusal does not name the knob: {error}"
        );
        Ok(())
    }

    /// The [`CELLS`] table covers every variant exactly once, so the sweep
    /// below skips none.
    #[test]
    fn cell_table_covers_every_variant() -> anyhow::Result<()> {
        use strum::EnumCount as _;

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
            seen.len() == BenchmarkFlags::COUNT,
            "table covers {} variants, enum has {}",
            seen.len(),
            BenchmarkFlags::COUNT
        );
        Ok(())
    }

    /// The JSON schema's arms are this table: one per cell, with its knobs.
    #[cfg(feature = "schemars")]
    #[test]
    fn the_json_schema_has_an_arm_per_cell_with_its_knobs() -> anyhow::Result<()> {
        crate::json_schema::assert_arms_are::<BenchmarkFlags>(CELLS)
    }

    /// The JSON schema's arms are the cells the deserializer takes, each with
    /// the [`PROBES`] it takes there.
    #[cfg(feature = "schemars")]
    #[test]
    fn the_json_schema_has_an_arm_per_accepted_cell_with_its_knobs() -> anyhow::Result<()> {
        let triples = crate::cell::every_triple()
            .map(|(b, r, m)| vec![b.to_string(), r.to_string(), m.to_string()]);
        crate::json_schema::assert_arms_are_accepted::<BenchmarkFlags, BenchmarkFlagRef>(
            crate::json_schema::TRIPLE,
            triples,
            PROBES,
        )
    }

    /// Walks every triple the axis types allow, so no variant escapes: each is
    /// named for the one triple that routes to it, and `axes()` reports it back.
    #[test]
    fn variants_are_named_for_their_cell() -> anyhow::Result<()> {
        use strum::EnumCount as _;

        let mut routed = std::collections::HashSet::new();
        let mut misnamed = Vec::new();
        for (b, r, m) in crate::cell::every_triple() {
            let Ok(flags) = BenchmarkFlags::try_from(cell_ref(b, r, m)) else {
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
            routed.len() == BenchmarkFlags::COUNT,
            "triples reach {} variants, the enum has {}",
            routed.len(),
            BenchmarkFlags::COUNT
        );
        anyhow::ensure!(
            misnamed.is_empty(),
            "misnamed variants: {}",
            misnamed.join(", ")
        );
        Ok(())
    }

    /// The value check compares against the authored wire: a dropped field is
    /// absent from both sides of a `parse(to_string)` round-trip.
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
                let (got, want) = (emitted.get(field), authored.get(field));
                anyhow::ensure!(
                    got == want,
                    "{benchmark}/{runtime}/{model}: `{field}` was accepted but came back as {got:?}, authored {want:?}",
                );
            }
        }
        Ok(())
    }
}
