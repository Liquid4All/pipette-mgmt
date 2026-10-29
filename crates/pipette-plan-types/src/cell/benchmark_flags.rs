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
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputLlamacppCliStockToolsGgufText {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyLlamacppCliStockToolsGgufText {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // llama.cpp CLI — server cells.
    EvalLlamacppCliStockToolsGgufText {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    VlThroughputLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// Speech recognition through llama-server: the same two knobs as
    /// `vl_throughput`, since it is the same request shape and the same gate.
    AsrThroughputLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// Peak-memory VL cell: `http_timeout_seconds` bounds one request,
    /// `bench_timeout_seconds` the whole workload. No `readiness`: one
    /// unrepeated, thermal-independent measurement.
    VlMaxMemoryLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
    },
    /// Peak-memory cell for speech recognition: the same two timeouts and no
    /// `readiness`, for the reasons the VL cell gives.
    AsrMaxMemoryLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
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
    /// A server-driven corpus traversal. `bench_timeout_seconds` bounds it end
    /// to end, gate waits included (100 on an MMSpec `test` subset): a budget
    /// sized from generation alone kills a healthy run.
    VlSpecDecodeLlamacppCliStockToolsGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// The same traversal with the drafter on, and the same knobs.
    VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // llama.cpp CLI — peak memory. A bare `llama-bench` subprocess, so the
    // wall-clock deadline is the whole surface.
    MaxMemoryUsageLlamacppCliStockToolsGgufText {
        bench_timeout_seconds: Option<PositiveSeconds>,
    },
    // MLX (macOS).
    PrefillThroughputMlxMacosPipetteMlx {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputMlxMacosPipetteMlx {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyMlxMacosPipetteMlx {
        readiness: Option<ReadinessOverrides>,
    },
    EvalMlxMacosPipetteMlx {
        http_timeout_seconds: Option<PositiveSeconds>,
    },
    /// The MLX twin of the llama.cpp CLI `vl_throughput` cell, with its knobs.
    VlThroughputMlxMacosPipetteMlx {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    /// The MLX twins of the llama.cpp CLI corpus cells, with the same knobs.
    VlSpecDecodeMlxMacosPipetteMlx {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeMlxMacosPipetteMlxSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // iOS (in-process): `readiness` only, with no HTTP client to bound or
    // server to watch. `eval` is absent for that reason, the memory kinds
    // because they do not gate.
    PrefillThroughputLlamacppIosPipetteGgufText {
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
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyLlamacppIosPipetteGgufText {
        readiness: Option<ReadinessOverrides>,
    },
    VlThroughputLlamacppIosPipetteGgufVision {
        readiness: Option<ReadinessOverrides>,
    },
    AsrThroughputLlamacppIosPipetteGgufVision {
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputMlxIosPipetteMlx {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputMlxIosPipetteMlx {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyMlxIosPipetteMlx {
        readiness: Option<ReadinessOverrides>,
    },
    // Android (in-process, as iOS): `readiness` alone. `max_memory_usage` does
    // not gate, and the app offers no `eval`.
    PrefillThroughputLlamacppApkPipetteGgufText {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputLlamacppApkPipetteGgufText {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyLlamacppApkPipetteGgufText {
        readiness: Option<ReadinessOverrides>,
    },
    VlThroughputLlamacppApkPipetteGgufVision {
        readiness: Option<ReadinessOverrides>,
    },
    // Torch servers, all over HTTP. Throughput cells are one streamed request
    // per rep, so they bound and gate as the latency cell does.
    PrefillThroughputDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    PrefillThroughputUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyDockerVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvVllmTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyDockerSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
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
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeDockerSglangTorchSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeUvSglangTorch {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlSpecDecodeUvSglangTorchSpeculative {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // OpenVINO (uv venv): a rep is a one-shot driver subprocess, so `readiness`
    // is the whole surface. `max_memory_usage` neither gates nor takes a
    // deadline, so it has no variant; `eval` and `vl` are unimplemented.
    PrefillThroughputUvOpenvinoOpenvino {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvOpenvinoOpenvino {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvOpenvinoOpenvino {
        readiness: Option<ReadinessOverrides>,
    },
    // OVMS — timing cells. An HTTP server like the torch ones, so the same
    // bound and gate. `eval` and the `vl` cells are unimplemented.
    PrefillThroughputOvmsOpenvino {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputOvmsOpenvino {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyOvmsOpenvino {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    // ONNX Runtime GenAI (uv venv): as OpenVINO, `readiness` alone; `eval` and
    // the `vl` cells are unimplemented.
    PrefillThroughputUvOnnxGenaiOnnxGenai {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputUvOnnxGenaiOnnxGenai {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyUvOnnxGenaiOnnxGenai {
        readiness: Option<ReadinessOverrides>,
    },
    // ONNX Runtime GenAI (iOS, in-process): `readiness` alone. No `eval`:
    // `OGATokenizer` applies no chat template, so the app cannot produce it.
    PrefillThroughputOnnxGenaiIosPipetteOnnxGenai {
        readiness: Option<ReadinessOverrides>,
    },
    DecodeThroughputOnnxGenaiIosPipetteOnnxGenai {
        readiness: Option<ReadinessOverrides>,
    },
    EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai {
        readiness: Option<ReadinessOverrides>,
    },
    // TensorRT Edge-LLM: the llama.cpp CLI VL cells' knobs. The engine build
    // runs before the server starts, so neither timeout covers it.
    VlThroughputTrtEdgellmGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        readiness: Option<ReadinessOverrides>,
    },
    VlMaxMemoryTrtEdgellmGgufVision {
        http_timeout_seconds: Option<PositiveSeconds>,
        bench_timeout_seconds: Option<PositiveSeconds>,
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
            | BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai { .. } => None,
        }
    }

    /// The whole-workload wall-clock deadline, on the cells that take one;
    /// `None` keeps the engine's default. [`Self::http_timeout`] bounds one
    /// request.
    pub fn bench_timeout(&self) -> Option<PositiveSeconds> {
        match self {
            BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText {
                bench_timeout_seconds,
            }
            | BenchmarkFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlx {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorch {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorch {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative {
                bench_timeout_seconds,
                ..
            }
            | BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision {
                bench_timeout_seconds,
                ..
            } => *bench_timeout_seconds,
            BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::EvalMlxMacosPipetteMlx { .. }
            | BenchmarkFlags::VlThroughputMlxMacosPipetteMlx { .. }
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
            | BenchmarkFlags::PrefillThroughputDockerVllmTorch { .. }
            | BenchmarkFlags::DecodeThroughputDockerVllmTorch { .. }
            | BenchmarkFlags::PrefillThroughputUvVllmTorch { .. }
            | BenchmarkFlags::DecodeThroughputUvVllmTorch { .. }
            | BenchmarkFlags::PrefillThroughputDockerSglangTorch { .. }
            | BenchmarkFlags::DecodeThroughputDockerSglangTorch { .. }
            | BenchmarkFlags::PrefillThroughputUvSglangTorch { .. }
            | BenchmarkFlags::DecodeThroughputUvSglangTorch { .. }
            | BenchmarkFlags::EndToEndLatencyDockerVllmTorch { .. }
            | BenchmarkFlags::EndToEndLatencyUvVllmTorch { .. }
            | BenchmarkFlags::EndToEndLatencyDockerSglangTorch { .. }
            | BenchmarkFlags::EndToEndLatencyUvSglangTorch { .. }
            | BenchmarkFlags::EvalDockerVllmTorch { .. }
            | BenchmarkFlags::EvalUvVllmTorch { .. }
            | BenchmarkFlags::VlmEvalUvVllmTorch { .. }
            | BenchmarkFlags::AsrEvalUvVllmTorch { .. }
            | BenchmarkFlags::EvalDockerSglangTorch { .. }
            | BenchmarkFlags::EvalUvSglangTorch { .. }
            | BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | BenchmarkFlags::PrefillThroughputOvmsOpenvino { .. }
            | BenchmarkFlags::DecodeThroughputOvmsOpenvino { .. }
            | BenchmarkFlags::EndToEndLatencyOvmsOpenvino { .. }
            | BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai { .. }
            | BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai { .. }
            | BenchmarkFlags::VlThroughputTrtEdgellmGgufVision { .. } => None,
        }
    }

    /// The host-readiness gate settings; `None` on cells that do not gate and
    /// when the cell set no override.
    pub fn readiness(&self) -> Option<&ReadinessOverrides> {
        match self {
            BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText { readiness }
            | BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText { readiness }
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
            | BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx { readiness }
            | BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx { readiness }
            | BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx { readiness }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { readiness, .. }
            | BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                readiness,
                ..
            }
            | BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText { readiness }
            | BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision { readiness }
            | BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision { readiness }
            | BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText { readiness }
            | BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText { readiness }
            | BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx { readiness }
            | BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx { readiness }
            | BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx { readiness }
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
            | BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino { readiness }
            | BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino { readiness }
            | BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino { readiness }
            | BenchmarkFlags::PrefillThroughputOvmsOpenvino { readiness, .. }
            | BenchmarkFlags::DecodeThroughputOvmsOpenvino { readiness, .. }
            | BenchmarkFlags::EndToEndLatencyOvmsOpenvino { readiness, .. }
            | BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai { readiness }
            | BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai { readiness }
            | BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai { readiness }
            | BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai { readiness }
            | BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai { readiness }
            | BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai { readiness }
            | BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText { readiness }
            | BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText { readiness }
            | BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText { readiness }
            | BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision { readiness }
            | BenchmarkFlags::VlThroughputTrtEdgellmGgufVision { readiness, .. } => {
                readiness.as_ref()
            }
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
            | BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. } => None,
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
    pub bench_timeout_seconds: Option<PositiveSeconds>,
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

    fn deny_bench_timeout(&self) -> Result<(), BenchmarkFlagsError> {
        self.deny(
            self.bench_timeout_seconds.is_some(),
            "bench_timeout_seconds",
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
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::MlxMacosPipette, M::MlxSpeculative) => {
                BenchmarkFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::DockerSglang, M::Torch) => {
                BenchmarkFlags::VlSpecDecodeDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::DockerSglang, M::TorchSpeculative) => {
                BenchmarkFlags::VlSpecDecodeDockerSglangTorchSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::UvSglang, M::Torch) => BenchmarkFlags::VlSpecDecodeUvSglangTorch {
                http_timeout_seconds: r.http_timeout_seconds,
                bench_timeout_seconds: r.bench_timeout_seconds,
                readiness: r.readiness,
            },
            (B::VlSpecDecode, R::UvSglang, M::TorchSpeculative) => {
                BenchmarkFlags::VlSpecDecodeUvSglangTorchSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVision) => {
                BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::LlamacppCliStockTools, M::GgufVisionSpeculative) => {
                BenchmarkFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // Timing cells.
            (B::PrefillThroughput, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputLlamacppCliStockToolsGgufText {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyLlamacppCliStockToolsGgufText {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputMlxMacosPipetteMlx {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputMlxMacosPipetteMlx {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::MlxMacosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyMlxMacosPipetteMlx {
                    readiness: r.readiness,
                }
            }
            // The iOS corpus cells: the gate, and nothing that bounds a
            // process this engine does not spawn.
            (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVision {
                    readiness: r.readiness,
                }
            }
            (B::VlSpecDecode, R::LlamacppIosPipette, M::GgufVisionSpeculative) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputLlamacppIosPipetteGgufText {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputLlamacppIosPipetteGgufText {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::LlamacppIosPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyLlamacppIosPipetteGgufText {
                    readiness: r.readiness,
                }
            }
            (B::VlThroughput, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlThroughputLlamacppIosPipetteGgufVision {
                    readiness: r.readiness,
                }
            }
            (B::AsrThroughput, R::LlamacppIosPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::AsrThroughputLlamacppIosPipetteGgufVision {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputMlxIosPipetteMlx {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputMlxIosPipetteMlx {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::MlxIosPipette, M::Mlx) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyMlxIosPipetteMlx {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputLlamacppApkPipetteGgufText {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputLlamacppApkPipetteGgufText {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::LlamacppApkPipette, M::GgufText) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyLlamacppApkPipetteGgufText {
                    readiness: r.readiness,
                }
            }
            (B::VlThroughput, R::LlamacppApkPipette, M::GgufVision) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlThroughputLlamacppApkPipetteGgufVision {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputUvOpenvinoOpenvino {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputUvOpenvinoOpenvino {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvOpenvino, M::Openvino) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyUvOpenvinoOpenvino {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::Ovms, M::Openvino) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputOvmsOpenvino {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::Ovms, M::Openvino) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputOvmsOpenvino {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::Ovms, M::Openvino) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyOvmsOpenvino {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputUvOnnxGenaiOnnxGenai {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputUvOnnxGenaiOnnxGenai {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvOnnxGenai, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyUvOnnxGenaiOnnxGenai {
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::DockerVllm, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::DockerVllm, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvVllm, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvVllm, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::DockerSglang, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::DockerSglang, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::UvSglang, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::UvSglang, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::PrefillThroughput, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::PrefillThroughputOnnxGenaiIosPipetteOnnxGenai {
                    readiness: r.readiness,
                }
            }
            (B::DecodeThroughput, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::DecodeThroughputOnnxGenaiIosPipetteOnnxGenai {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::OnnxGenaiIosPipette, M::OnnxGenai) => {
                r.deny_http()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyOnnxGenaiIosPipetteOnnxGenai {
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::DockerVllm, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvVllm, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::DockerSglang, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::EndToEndLatency, R::UvSglang, M::Torch) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::EndToEndLatencyUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // vl-throughput: HTTP timeout + readiness.
            (B::VlThroughput, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlThroughputLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // As vl_throughput.
            (B::AsrThroughput, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::AsrThroughputLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlThroughput, R::MlxMacosPipette, M::Mlx) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlThroughputMlxMacosPipetteMlx {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // vlm-eval: HTTP timeout + readiness, same shape as vl-throughput —
            // the vision tower runs thousands of images back-to-back, so the
            // readiness gate matters here as much as on the timing cells.
            (B::VlmEval, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            // asr-eval: the vlm_eval shape, clips instead of images.
            (B::AsrEval, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_bench_timeout()?;
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
                    bench_timeout_seconds: r.bench_timeout_seconds,
                }
            }
            // As vl_max_memory.
            (B::AsrMaxMemory, R::LlamacppCliStockTools, M::GgufVision) => {
                r.deny_readiness()?;
                BenchmarkFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                }
            }
            // TensorRT Edge-LLM: as the llama.cpp CLI VL cells above.
            (B::VlThroughput, R::TrtEdgellm, M::GgufVision) => {
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlThroughputTrtEdgellmGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    readiness: r.readiness,
                }
            }
            (B::VlMaxMemory, R::TrtEdgellm, M::GgufVision) => {
                r.deny_readiness()?;
                BenchmarkFlags::VlMaxMemoryTrtEdgellmGgufVision {
                    http_timeout_seconds: r.http_timeout_seconds,
                    bench_timeout_seconds: r.bench_timeout_seconds,
                }
            }
            // max_memory_usage: the subprocess deadline only (see module docs).
            (B::MaxMemoryUsage, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_http()?;
                r.deny_readiness()?;
                BenchmarkFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText {
                    bench_timeout_seconds: r.bench_timeout_seconds,
                }
            }
            // eval: HTTP timeout (no readiness).
            (B::Eval, R::LlamacppCliStockTools, M::GgufText) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EvalLlamacppCliStockToolsGgufText {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::MlxMacosPipette, M::Mlx) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EvalMlxMacosPipetteMlx {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::DockerVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EvalDockerVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::UvVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EvalUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::VlmEval, R::UvVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::VlmEvalUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::AsrEval, R::UvVllm, M::Torch) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::AsrEvalUvVllmTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::DockerSglang, M::Torch) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EvalDockerSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
                }
            }
            (B::Eval, R::UvSglang, M::Torch) => {
                r.deny_readiness()?;
                r.deny_bench_timeout()?;
                BenchmarkFlags::EvalUvSglangTorch {
                    http_timeout_seconds: r.http_timeout_seconds,
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
            bench_timeout_seconds: f.bench_timeout(),
            _doomloop_obsolete: None,
            readiness: f.readiness().cloned(),
        }
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;

    fn prefill_flags(readiness: Option<ReadinessOverrides>) -> BenchmarkFlags {
        BenchmarkFlags::PrefillThroughputLlamacppCliStockToolsGgufText { readiness }
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
            bench_timeout_seconds: None,
            _doomloop_obsolete: None,
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
            bench_timeout_seconds: None,
            _doomloop_obsolete: None,
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
        r.bench_timeout_seconds = Some(PositiveSeconds::try_new(2400)?);
        let flags = BenchmarkFlags::try_from(r)?;
        assert_eq!(flags.http_timeout(), Some(PositiveSeconds::try_new(900)?));
        assert_eq!(flags.bench_timeout(), Some(PositiveSeconds::try_new(2400)?));
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
                BenchmarkType::MaxMemoryUsage,
                RuntimeType::LlamacppApkPipette,
                ModelType::GgufText,
            ),
            cell_ref(
                BenchmarkType::Eval,
                RuntimeType::LlamacppIosPipette,
                ModelType::GgufText,
            ),
            cell_ref(
                BenchmarkType::VlMaxMemory,
                RuntimeType::LlamacppIosPipette,
                ModelType::GgufVision,
            ),
            cell_ref(
                BenchmarkType::AsrMaxMemory,
                RuntimeType::LlamacppIosPipette,
                ModelType::GgufVision,
            ),
            cell_ref(
                BenchmarkType::MaxMemoryUsage,
                RuntimeType::MlxIosPipette,
                ModelType::Mlx,
            ),
            cell_ref(
                BenchmarkType::MaxMemoryUsage,
                RuntimeType::UvOpenvino,
                ModelType::Openvino,
            ),
            cell_ref(
                BenchmarkType::MaxMemoryUsage,
                RuntimeType::Ovms,
                ModelType::Openvino,
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
    #[case::text_bench("max_memory_usage", "gguf_text", "bench_timeout_seconds")]
    #[case::vl_bench("vl_max_memory", "gguf_vision", "bench_timeout_seconds")]
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
        ("bench_timeout_seconds", "1800"),
        ("readiness", "{ max_wait_secs = 120 }"),
    ];

    /// Every cell with a variant and its accepted fields. Transcribed by hand,
    /// not derived, so a variant's field change without an edit here fails.
    #[rustfmt::skip]
    const CELLS: &[(&str, &str, &str, &[&str])] = &[
        ("prefill_throughput", "llamacpp_cli_stock_tools", "gguf_text", &["readiness"]),
        ("decode_throughput", "llamacpp_cli_stock_tools", "gguf_text", &["readiness"]),
        ("end_to_end_latency", "llamacpp_cli_stock_tools", "gguf_text",
         &["http_timeout_seconds", "readiness"]),
        ("asr_throughput", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "readiness"]),
        ("prefill_throughput", "mlx_macos_pipette", "mlx", &["readiness"]),
        ("decode_throughput", "mlx_macos_pipette", "mlx", &["readiness"]),
        ("end_to_end_latency", "mlx_macos_pipette", "mlx", &["readiness"]),
        ("prefill_throughput", "llamacpp_ios_pipette", "gguf_text", &["readiness"]),
        ("decode_throughput", "llamacpp_ios_pipette", "gguf_text", &["readiness"]),
        ("end_to_end_latency", "llamacpp_ios_pipette", "gguf_text", &["readiness"]),
        ("vl_throughput", "llamacpp_ios_pipette", "gguf_vision", &["readiness"]),
        ("asr_throughput", "llamacpp_ios_pipette", "gguf_vision", &["readiness"]),
        ("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision", &["readiness"]),
        ("vl_spec_decode", "llamacpp_ios_pipette", "gguf_vision_speculative",
         &["readiness"]),
        ("prefill_throughput", "mlx_ios_pipette", "mlx", &["readiness"]),
        ("decode_throughput", "mlx_ios_pipette", "mlx", &["readiness"]),
        ("end_to_end_latency", "mlx_ios_pipette", "mlx", &["readiness"]),
        ("prefill_throughput", "llamacpp_apk_pipette", "gguf_text", &["readiness"]),
        ("decode_throughput", "llamacpp_apk_pipette", "gguf_text", &["readiness"]),
        ("end_to_end_latency", "llamacpp_apk_pipette", "gguf_text", &["readiness"]),
        ("vl_throughput", "llamacpp_apk_pipette", "gguf_vision", &["readiness"]),
        ("prefill_throughput", "docker_vllm", "torch", &["http_timeout_seconds", "readiness"]),
        ("decode_throughput", "docker_vllm", "torch", &["http_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "docker_vllm", "torch", &["http_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_vllm", "torch", &["http_timeout_seconds", "readiness"]),
        ("decode_throughput", "uv_vllm", "torch", &["http_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "uv_vllm", "torch", &["http_timeout_seconds", "readiness"]),
        ("prefill_throughput", "docker_sglang", "torch", &["http_timeout_seconds", "readiness"]),
        ("decode_throughput", "docker_sglang", "torch", &["http_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "docker_sglang", "torch", &["http_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_sglang", "torch", &["http_timeout_seconds", "readiness"]),
        ("decode_throughput", "uv_sglang", "torch", &["http_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "uv_sglang", "torch", &["http_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_openvino", "openvino", &["readiness"]),
        ("decode_throughput", "uv_openvino", "openvino", &["readiness"]),
        ("end_to_end_latency", "uv_openvino", "openvino", &["readiness"]),
        ("prefill_throughput", "ovms", "openvino", &["http_timeout_seconds", "readiness"]),
        ("decode_throughput", "ovms", "openvino", &["http_timeout_seconds", "readiness"]),
        ("end_to_end_latency", "ovms", "openvino", &["http_timeout_seconds", "readiness"]),
        ("prefill_throughput", "uv_onnx_genai", "onnx_genai", &["readiness"]),
        ("decode_throughput", "uv_onnx_genai", "onnx_genai", &["readiness"]),
        ("end_to_end_latency", "uv_onnx_genai", "onnx_genai", &["readiness"]),
        ("prefill_throughput", "onnx_genai_ios_pipette", "onnx_genai", &["readiness"]),
        ("decode_throughput", "onnx_genai_ios_pipette", "onnx_genai", &["readiness"]),
        ("end_to_end_latency", "onnx_genai_ios_pipette", "onnx_genai", &["readiness"]),
        ("vl_throughput", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "readiness"]),
        // vlm-eval: an HTTP server run, same shape as vl_throughput — the
        // vision tower runs thousands of images back-to-back, so the
        // readiness gate matters here as much as on the timing cells.
        ("vlm_eval", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "readiness"]),
        ("asr_eval", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "readiness"]),
        ("vl_throughput", "mlx_macos_pipette", "mlx", &["http_timeout_seconds", "readiness"]),
        ("vl_max_memory", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "bench_timeout_seconds"]),
        ("vl_throughput", "trt_edgellm", "gguf_vision", &["http_timeout_seconds", "readiness"]),
        ("vl_max_memory", "trt_edgellm", "gguf_vision",
         &["http_timeout_seconds", "bench_timeout_seconds"]),
        ("asr_max_memory", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "bench_timeout_seconds"]),
        ("vl_spec_decode", "llamacpp_cli_stock_tools", "gguf_vision",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "llamacpp_cli_stock_tools", "gguf_vision_speculative",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "mlx_macos_pipette", "mlx",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "mlx_macos_pipette", "mlx_speculative",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("max_memory_usage", "llamacpp_cli_stock_tools", "gguf_text",
         &["bench_timeout_seconds"]),
        ("eval", "llamacpp_cli_stock_tools", "gguf_text", &["http_timeout_seconds"]),
        ("eval", "mlx_macos_pipette", "mlx", &["http_timeout_seconds"]),
        ("eval", "docker_vllm", "torch", &["http_timeout_seconds"]),
        ("eval", "uv_vllm", "torch", &["http_timeout_seconds"]),
        ("vlm_eval", "uv_vllm", "torch", &["http_timeout_seconds"]),
        ("asr_eval", "uv_vllm", "torch", &["http_timeout_seconds"]),
        ("eval", "docker_sglang", "torch", &["http_timeout_seconds"]),
        ("eval", "uv_sglang", "torch", &["http_timeout_seconds"]),
        ("vl_spec_decode", "docker_sglang", "torch",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "docker_sglang", "torch_speculative",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "uv_sglang", "torch",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
        ("vl_spec_decode", "uv_sglang", "torch_speculative",
         &["http_timeout_seconds", "bench_timeout_seconds", "readiness"]),
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
