//! One cell: a (benchmark, runtime, model) point of a plan's matrix, its knobs,
//! and the job body a client is served for it. The matrix helpers are public so
//! another crate can expand a `SchedulerPlan` by the same rules.

mod benchmark_flags;
mod context_bound;
mod model_flags;
mod runtime_flags;

/// Flags each runtime's benchmarks fix internally, the one source of that
/// policy. Here, not beside the executors, because they depend on plan-types.
mod reserved_flags;

/// Reading a result's recorded flags back against the cell its row names.
mod record {
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    /// Why recorded flags did not read back against their cell.
    #[derive(Debug, thiserror::Error)]
    pub enum FlagsRecordError<E> {
        #[error("flags are not JSON: {0}")]
        Json(#[from] serde_json::Error),
        #[error("flags are not a JSON object")]
        NotAnObject,
        /// The record names its cell in its own columns, so the flags may not.
        #[error("`{key}` is the cell's, not a flag")]
        AxisNotAccepted { key: &'static str },
        #[error(transparent)]
        Flags(E),
    }

    /// Recorded knobs as the flat `*FlagRef` they came from, the cell's `axes`
    /// added; `None` for an empty object.
    pub(super) fn with_cell<R: DeserializeOwned, E>(
        json: &str,
        axes: &[(&'static str, Value)],
    ) -> Result<Option<R>, FlagsRecordError<E>> {
        let Value::Object(mut fields) = serde_json::from_str::<Value>(json)? else {
            return Err(FlagsRecordError::NotAnObject);
        };
        if fields.is_empty() {
            return Ok(None);
        }
        axes.iter().try_for_each(|(key, derived)| {
            if fields.contains_key(*key) {
                return Err(FlagsRecordError::AxisNotAccepted { key });
            }
            fields.insert((*key).to_owned(), derived.clone());
            Ok(())
        })?;
        Ok(Some(serde_json::from_value(Value::Object(fields))?))
    }

    #[cfg(test)]
    mod tests {
        use serde_json::json;

        use super::*;
        use crate::{BenchmarkFlagRef, BenchmarkFlagsError};

        #[rstest::rstest]
        #[case::empty("{}", "none")]
        #[case::knobs(r#"{"http_timeout_seconds":600}"#, "read")]
        #[case::cell_key(r#"{"benchmark_type":"eval"}"#, "axis")]
        #[case::not_an_object("[]", "not_an_object")]
        #[case::unknown_field(r#"{"cooldown_seconds":240}"#, "json")]
        #[case::not_json("enable_thinking=true", "json")]
        fn recorded_knobs_read_with_their_cell(#[case] json: &str, #[case] expected: &str) {
            let axes = [
                ("runtime_type", json!("llamacpp_cli_stock_tools")),
                ("model_type", json!("gguf_text")),
                ("benchmark_type", json!("eval")),
            ];
            let read = with_cell::<BenchmarkFlagRef, BenchmarkFlagsError>(json, &axes);
            let got = match read {
                Ok(None) => "none",
                Ok(Some(_)) => "read",
                Err(FlagsRecordError::AxisNotAccepted { .. }) => "axis",
                Err(FlagsRecordError::NotAnObject) => "not_an_object",
                Err(FlagsRecordError::Json(_)) => "json",
                Err(FlagsRecordError::Flags(_)) => "flags",
            };
            assert_eq!(got, expected);
        }
    }
}

pub use self::record::FlagsRecordError;
pub use benchmark_flags::{
    BenchmarkFlagRef, BenchmarkFlags, BenchmarkFlagsError, BenchmarkFlagsRecordError,
    ReadinessOverrides,
};
pub use context_bound::EvalContextError;
pub use model_flags::{
    ModelFlagRef, ModelFlags, ModelFlagsError, ModelFlagsRecordError, ReasoningEffort,
};
pub use runtime_flags::{
    AsrEvalLlamacppCliStockToolsGgufVision, AsrMaxMemoryLlamacppCliStockToolsGgufVision,
    AsrThroughputLlamacppCliStockToolsGgufVision, DecodeThroughputLlamacppCliStockToolsGgufText,
    DecodeThroughputOvmsOpenvino, DecodeThroughputUvOpenvinoOpenvino,
    EndToEndLatencyLlamacppCliStockToolsGgufText, EndToEndLatencyOvmsOpenvino,
    EndToEndLatencyUvOpenvinoOpenvino, EvalLlamacppCliStockToolsGgufText,
    MaxMemoryUsageLlamacppCliStockToolsGgufText, MaxMemoryUsageOvmsOpenvino,
    MaxMemoryUsageUvOpenvinoOpenvino, PrefillThroughputLlamacppCliStockToolsGgufText,
    PrefillThroughputOvmsOpenvino, PrefillThroughputUvOpenvinoOpenvino, RuntimeFlagRef,
    RuntimeFlags, RuntimeFlagsError, TrtEdgellmGgufVision,
    VlMaxMemoryLlamacppCliStockToolsGgufVision, VlSpecDecodeLlamacppCliStockTools,
    VlThroughputLlamacppCliStockToolsGgufVision, VlmEvalLlamacppCliStockToolsGgufVision,
    resolve_speculation,
};

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{BenchmarkId, BenchmarkType, Model, ModelType, Runtime, RuntimeType};

/// True iff `model`'s format matches what `runtime` can load.
pub fn is_compatible(model: &Model, runtime: &Runtime) -> bool {
    types_compatible(ModelType::of(model), RuntimeType::of(runtime))
}

/// Each arm is an accepted pair; `_ => false` fails closed, so a new variant
/// needs a row. [`runnable_type_pairs_of`] counts cells by type, so
/// compatibility must depend on the types alone.
fn types_compatible(model: ModelType, runtime: RuntimeType) -> bool {
    use ModelType::*;
    use RuntimeType::*;
    match (model, runtime) {
        (GgufText, LlamacppCliStockTools | LlamacppApkPipette | LlamacppIosPipette) => true,
        (GgufVision, LlamacppCliStockTools | LlamacppApkPipette | LlamacppIosPipette) => true,
        // The engines are built from the GGUF vision pair.
        (GgufVision, TrtEdgellm) => true,
        // Not the apk: its engine has no drafter and no vision tower.
        (GgufVisionSpeculative, LlamacppCliStockTools | LlamacppIosPipette) => true,
        (Mlx, MlxMacosPipette | MlxIosPipette) => true,
        // macOS only: the iOS MLX engine loads one bundle and has no drafter.
        (MlxSpeculative, MlxMacosPipette) => true,
        (Torch, DockerVllm | DockerSglang | UvVllm | UvSglang) => true,
        // SGLang only: its DSpark drafter is the one torch spec-decode path
        // this client measures.
        (TorchSpeculative, DockerSglang | UvSglang) => true,
        (AppleFoundationText, AppleFoundation) => true,
        (Openvino, UvOpenvino | Ovms) => true,
        (OnnxGenai, UvOnnxGenai | OnnxGenaiIosPipette) => true,
        _ => false,
    }
}

/// Why a variant does not expand into cells. Shared by both plan formats, so the
/// caller adds which variant: only it knows the index.
// Boxed: `Runtime` is ~640 bytes and trips clippy::large_enum_variant; the
// error path is cold.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VariantError {
    #[error("model {0} has no compatible runtime in this variant")]
    ModelHasNoCompatibleRuntime(Box<Model>),
    #[error("runtime {0} has no compatible model in this variant")]
    RuntimeHasNoCompatibleModel(Box<Runtime>),
    #[error("benchmarks must be set on the variant or at the plan root")]
    NoBenchmarks,
    #[error("duplicate runtime_flags entry for the same cell triple")]
    DuplicateRuntimeFlags,
    #[error("duplicate model_flags entry for the same cell")]
    DuplicateModelFlags,
    #[error("duplicate benchmark_flags entry for the same cell")]
    DuplicateBenchmarkFlags,
    #[error(
        "a runtime_flags entry matches no cell in the variant \
         (no benchmark\u{d7}runtime\u{d7}model triple satisfies it)"
    )]
    RuntimeFlagsMatchNoCell,
    #[error(
        "a model_flags entry matches no cell in the variant \
         (no eval benchmark\u{d7}model pair satisfies it)"
    )]
    ModelFlagsMatchNoCell,
    #[error(
        "a benchmark_flags entry matches no cell in the variant \
         (no benchmark\u{d7}runtime\u{d7}model triple satisfies it)"
    )]
    BenchmarkFlagsMatchNoCell,
}

/// The (model, runtime) pairs a variant expands into: the cartesian product
/// filtered by [`is_compatible`]. Errors on an orphan model or runtime with no
/// counterpart, almost certainly an authoring mistake; other incompatible
/// pairs are dropped.
pub fn runnable_pairs_of<'a>(
    models: &'a [Model],
    runtimes: &'a [Runtime],
) -> Result<Vec<(&'a Model, &'a Runtime)>, VariantError> {
    runnable_type_pairs_of(models, runtimes)?;
    Ok(models
        .iter()
        .flat_map(|m| {
            runtimes
                .iter()
                .filter(move |r| is_compatible(m, r))
                .map(move |r| (m, r))
        })
        .collect())
}

/// One of [`runnable_pairs_of`]'s pairs per (model type, runtime type), with
/// how many pairs share those types. Compatibility and flags see only the
/// types, so this checks and counts a variant without building its product.
pub fn runnable_type_pairs_of<'a>(
    models: &'a [Model],
    runtimes: &'a [Runtime],
) -> Result<Vec<(&'a Model, &'a Runtime, usize)>, VariantError> {
    let models = first_of_each_type(models, ModelType::of);
    let runtimes = first_of_each_type(runtimes, RuntimeType::of);
    if let Some(&(model, _)) = models
        .iter()
        .find(|(m, _)| !runtimes.iter().any(|(r, _)| is_compatible(m, r)))
    {
        return Err(VariantError::ModelHasNoCompatibleRuntime(Box::new(
            model.clone(),
        )));
    }
    if let Some(&(runtime, _)) = runtimes
        .iter()
        .find(|(r, _)| !models.iter().any(|(m, _)| is_compatible(m, r)))
    {
        return Err(VariantError::RuntimeHasNoCompatibleModel(Box::new(
            runtime.clone(),
        )));
    }
    Ok(models
        .iter()
        .flat_map(|&(m, m_count)| {
            runtimes
                .iter()
                .filter(move |(r, _)| is_compatible(m, r))
                .map(move |&(r, r_count)| (m, r, m_count.saturating_mul(r_count)))
        })
        .collect())
}

/// The first item of each type in document order, with how many items share
/// its type.
fn first_of_each_type<T, K: PartialEq>(items: &[T], type_of: fn(&T) -> K) -> Vec<(&T, usize)> {
    items.iter().fold(Vec::new(), |mut firsts, item| {
        match firsts
            .iter_mut()
            .find(|(first, _)| type_of(first) == type_of(item))
        {
            Some((_, count)) => *count += 1,
            None => firsts.push((item, 1)),
        }
        firsts
    })
}

/// Pick the runtime-flags entry matching a cell's `(benchmark, runtime,
/// model)`. Unique only if `entries` passed [`validate_variant_flag_cells`];
/// otherwise the first match wins.
pub fn resolve_runtime_flags(
    entries: &[RuntimeFlags],
    benchmark: Option<BenchmarkType>,
    runtime: &Runtime,
    model: &Model,
) -> Option<RuntimeFlags> {
    let bt = benchmark?;
    entries
        .iter()
        .find(|f| f.matches(bt, runtime, model))
        .cloned()
}

/// Pick the model-flags entry matching a cell's `(benchmark, model)`; `None`
/// off eval. Unique only if `entries` passed [`validate_variant_flag_cells`];
/// otherwise the first match wins.
pub fn resolve_model_flags(
    entries: &[ModelFlags],
    benchmark: Option<BenchmarkType>,
    model: &Model,
) -> Option<ModelFlags> {
    let bt = benchmark?;
    entries.iter().find(|f| f.matches(bt, model)).cloned()
}

/// Pick the benchmark-flags entry matching a cell's `(benchmark, runtime,
/// model)`. Unique only if `entries` passed [`validate_variant_flag_cells`];
/// otherwise the first match wins.
pub fn resolve_benchmark_flags(
    entries: &[BenchmarkFlags],
    benchmark: Option<BenchmarkType>,
    runtime: &Runtime,
    model: &Model,
) -> Option<BenchmarkFlags> {
    let bt = benchmark?;
    entries
        .iter()
        .find(|f| f.matches(bt, runtime, model))
        .cloned()
}

/// A variant's benchmark list: its own if set, else the plan's. Shared by both
/// plan formats so the fallback and its error text can't drift apart.
pub fn resolve_benchmarks<'a>(
    variant_benchmarks: Option<&'a [BenchmarkId]>,
    plan_benchmarks: Option<&'a [BenchmarkId]>,
) -> Result<&'a [BenchmarkId], VariantError> {
    variant_benchmarks
        .or(plan_benchmarks)
        .ok_or(VariantError::NoBenchmarks)
}

/// The benchmark type by id prefix (no synced catalog at this layer); `None`
/// for an unknown kind, which gets no typed flags. A server plan types by its
/// catalog instead; this types the local-dispatch [`crate::Plan`] and plans
/// filed before their manifest recorded `benchmark_types`.
pub fn benchmark_type_of(benchmark: &BenchmarkId) -> Option<BenchmarkType> {
    BenchmarkType::from_id(benchmark.as_ref())
}

/// Whether two entries share a cell key: an entry's identity is its cell.
fn has_duplicate_cell<K: Eq + std::hash::Hash>(keys: impl IntoIterator<Item = K>) -> bool {
    let mut seen = HashSet::new();
    !keys.into_iter().all(|key| seen.insert(key))
}

/// Validate one variant's per-cell flags: no duplicate cell keys, and every
/// entry matches at least one runnable cell. Sound only when `pairs` comes
/// from [`runnable_pairs_of`] or [`runnable_type_pairs_of`] (an unfiltered
/// product lets an entry on an incompatible pair pass) and `benchmark_types`
/// from [`resolve_benchmarks`] through [`benchmark_type_of`].
pub fn validate_variant_flag_cells(
    pairs: &[(&Model, &Runtime)],
    benchmark_types: &[BenchmarkType],
    runtime_flags: &[RuntimeFlags],
    model_flags: &[ModelFlags],
    benchmark_flags: &[BenchmarkFlags],
) -> Result<(), VariantError> {
    if has_duplicate_cell(runtime_flags.iter().map(RuntimeFlags::axes)) {
        return Err(VariantError::DuplicateRuntimeFlags);
    }
    if !runtime_flags.iter().all(|flag| {
        benchmark_types
            .iter()
            .any(|&bt| pairs.iter().any(|(m, r)| flag.matches(bt, r, m)))
    }) {
        return Err(VariantError::RuntimeFlagsMatchNoCell);
    }

    if has_duplicate_cell(model_flags.iter().map(ModelFlags::axes)) {
        return Err(VariantError::DuplicateModelFlags);
    }
    if !model_flags.iter().all(|flag| {
        benchmark_types
            .iter()
            .any(|&bt| pairs.iter().any(|(m, _)| flag.matches(bt, m)))
    }) {
        return Err(VariantError::ModelFlagsMatchNoCell);
    }

    if has_duplicate_cell(benchmark_flags.iter().map(BenchmarkFlags::axes)) {
        return Err(VariantError::DuplicateBenchmarkFlags);
    }
    if !benchmark_flags.iter().all(|flag| {
        benchmark_types
            .iter()
            .any(|&bt| pairs.iter().any(|(m, r)| flag.matches(bt, r, m)))
    }) {
        return Err(VariantError::BenchmarkFlagsMatchNoCell);
    }
    Ok(())
}

/// Work payload served to a pipette client: benchmark × model × runtime + flags.
/// It carries no plan routing, store-resolved benchmark body or host paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientRunSpec {
    pub benchmark: BenchmarkId,
    pub model: Model,
    pub runtime: Runtime,
    /// Typed runtime flags for this cell's `(benchmark, runtime, model)`. A job
    /// body may omit an unset flag group.
    #[serde(default)]
    pub runtime_flags: Option<RuntimeFlags>,
    /// Model-generation flags for `(benchmark, model)`. Eval and vlm_eval only.
    #[serde(default)]
    pub model_flags: Option<ModelFlags>,
    /// Eval/vl-run flags (HTTP timeout, readiness). `None` when unset.
    #[serde(default)]
    pub benchmark_flags: Option<BenchmarkFlags>,
}

impl ClientRunSpec {
    /// The spec as a record carries it: no HF token, and env values only under
    /// `GGML_*` ([`RuntimeFlags::for_record`]).
    pub fn for_record(&self) -> Self {
        // Every field by name, so a new one needs a call on whether it is a secret.
        Self {
            benchmark: self.benchmark.clone(),
            model: self.model.without_auth_token(),
            runtime: self.runtime.clone(),
            runtime_flags: self.runtime_flags.as_ref().map(RuntimeFlags::for_record),
            model_flags: self.model_flags.clone(),
            benchmark_flags: self.benchmark_flags.clone(),
        }
    }

    /// Whether the runtime loads the model, and `runtime_flags`, if any, are
    /// for that runtime and model. The benchmark is the client's to resolve.
    pub fn validate(&self) -> Result<(), RunSpecError> {
        let runtime = RuntimeType::of(&self.runtime);
        let model = ModelType::of(&self.model);
        if !is_compatible(&self.model, &self.runtime) {
            return Err(RunSpecError::Incompatible { model, runtime });
        }
        if let Some(flags) = &self.runtime_flags {
            let (_, flags_runtime, flags_model) = flags.axes();
            if (flags_runtime, flags_model) != (runtime, model) {
                return Err(RunSpecError::RuntimeFlagsForAnotherPair {
                    flags_runtime,
                    flags_model,
                    runtime,
                    model,
                });
            }
        }
        Ok(())
    }
}

/// Why no host can run a [`ClientRunSpec`]. Names types, never a model or a
/// runtime: either may carry a token.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RunSpecError {
    #[error("runtime `{runtime}` cannot load a `{model}` model")]
    Incompatible {
        model: ModelType,
        runtime: RuntimeType,
    },
    #[error(
        "`runtime_flags` are for `{flags_runtime}` × `{flags_model}`, and the spec is \
         `{runtime}` × `{model}`"
    )]
    RuntimeFlagsForAnotherPair {
        flags_runtime: RuntimeType,
        flags_model: ModelType,
        runtime: RuntimeType,
        model: ModelType,
    },
}

/// Every `(benchmark, runtime, model)` triple the axis types allow, cell or not.
#[cfg(test)]
fn every_triple() -> impl Iterator<Item = (BenchmarkType, crate::RuntimeType, crate::ModelType)> {
    use strum::IntoEnumIterator as _;

    BenchmarkType::ALL.iter().flat_map(|&benchmark| {
        crate::RuntimeType::iter().flat_map(move |runtime| {
            crate::ModelType::iter().map(move |model| (benchmark, runtime, model))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{app_source, cli_release, source_repo};
    use crate::*;

    fn hf_model(repo: &str) -> anyhow::Result<ModelSource> {
        Ok(ModelSource::HuggingFace {
            repo: HfRepo::parse_org_repo(repo)?,
            prefix: None,
        })
    }

    fn openvino_model() -> anyhow::Result<Model> {
        Ok(Model::Openvino(Openvino {
            source: hf_model("LiquidAI/LFM2.5-350M-ov")?,
        }))
    }

    fn onnx_genai_model() -> anyhow::Result<Model> {
        Ok(Model::OnnxGenai(OnnxGenai {
            source: hf_model("microsoft/Phi-3-mini-4k-instruct-onnx")?,
        }))
    }

    fn mlx_model() -> anyhow::Result<Model> {
        Ok(Model::Mlx(Mlx {
            source: hf_model("mlx-community/Phi-3-mini-4k-instruct-4bit")?,
        }))
    }

    fn torch_model() -> anyhow::Result<Model> {
        Ok(Model::Torch(Torch {
            source: hf_model("LiquidAI/LFM2.5-350M")?,
        }))
    }

    fn openvino_runtime() -> anyhow::Result<Runtime> {
        Ok(Runtime::UvOpenvino(UvOpenvino {
            server_version: UvServerVersion::try_new("2026.2.1".to_owned())?,
            python_version: UvPythonVersion::try_new("3.11".to_owned())?,
            source: UvRuntimeSource::PipRequirementsText {
                contents: NonEmptyString::try_new("openvino-genai==2026.2.1.0\n".to_owned())?,
                install_flags: None,
            },
        }))
    }

    fn onnx_genai_runtime() -> anyhow::Result<Runtime> {
        Ok(Runtime::UvOnnxGenai(UvOnnxGenai {
            server_version: UvServerVersion::try_new("0.16.0".to_owned())?,
            execution_provider: OnnxExecutionProvider::Cpu,
            python_version: UvPythonVersion::try_new("3.11".to_owned())?,
            source: UvRuntimeSource::PipRequirementsText {
                contents: NonEmptyString::try_new("onnxruntime-genai==0.16.0\n".to_owned())?,
                install_flags: None,
            },
        }))
    }

    fn onnx_genai_ios_runtime() -> anyhow::Result<Runtime> {
        Ok(Runtime::OnnxGenaiIosPipette(OnnxGenaiIosPipette {
            packages: OnnxGenaiStack {
                onnxruntime: source_repo("github.com/microsoft/onnxruntime", "v1.23.0")?,
                onnxruntime_genai: source_repo(
                    "github.com/microsoft/onnxruntime-genai",
                    "v0.15.2",
                )?,
            },
            flavor: OnnxGenaiIosPipetteFlavor::IosArm64,
            private_apis: PrivateApis::default(),
        }))
    }

    fn mlx_ios_runtime() -> anyhow::Result<Runtime> {
        Ok(Runtime::MlxIosPipette(MlxIosPipette {
            packages: MlxSwiftStack {
                mlx_swift: source_repo("github.com/ml-explore/mlx-swift", "0.25.6")?,
                mlx_swift_lm: source_repo("github.com/ml-explore/mlx-swift-examples", "2.25.6")?,
                swift_transformers: source_repo(
                    "github.com/huggingface/swift-transformers",
                    "0.1.22",
                )?,
            },
            flavor: MlxIosPipetteFlavor::IosArm64,
            private_apis: PrivateApis::default(),
        }))
    }

    /// `flavor` is a parameter because the llama.cpp build is the variable
    /// under test in `openvino_ir_pairs_only_with_the_openvino_runtime`.
    fn llamacpp(version: &str, flavor: LlamaCppFlavor) -> anyhow::Result<Runtime> {
        Ok(Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: LlamacppCliStockToolsSource::GithubRelease(cli_release(version)?),
            flavor,
        }))
    }

    /// OpenVINO IR pairs with the OpenVINO runtime only, not with
    /// `linux-x64-openvino`, a llama.cpp build that still eats GGUF.
    #[test]
    fn openvino_ir_pairs_only_with_the_openvino_runtime() -> anyhow::Result<()> {
        let model = openvino_model()?;
        let openvino = openvino_runtime()?;
        assert!(is_compatible(&model, &openvino));

        let llamacpp = llamacpp("b9305", LlamaCppFlavor::LinuxX64Openvino)?;
        assert!(!is_compatible(&model, &llamacpp));
        assert!(!is_compatible(
            &model,
            &Runtime::AppleFoundation(Default::default())
        ));

        // And the runtime is equally narrow: it loads IR, not safetensors.
        assert!(!is_compatible(&torch_model()?, &openvino));
        Ok(())
    }

    /// OVMS serves the same IR directory `uv_openvino` does, and nothing else:
    /// not safetensors, and not the GGUF a llama.cpp OpenVINO build eats.
    #[test]
    fn ovms_pairs_only_with_openvino_ir() -> anyhow::Result<()> {
        let ovms = Runtime::Ovms(Ovms {
            source: OvmsSource::GithubRelease(OvmsSourceRepository {
                repository_url: RepositoryUrl::new("github.com/openvinotoolkit/model_server"),
                repository_version: NonEmptyString::try_new("v2026.4.0".to_owned())?,
            }),
            flavor: OvmsFlavor::WindowsX64,
        });
        assert!(is_compatible(&openvino_model()?, &ovms));

        let gguf: Model = toml::from_str(GGUF_TEXT)?;
        assert!(!is_compatible(&gguf, &ovms));
        assert!(!is_compatible(&torch_model()?, &ovms));
        assert!(!is_compatible(&onnx_genai_model()?, &ovms));
        Ok(())
    }

    /// TensorRT Edge-LLM builds its engines from a GGUF vision pair, and
    /// nothing else: not a text GGUF, not a drafter pair, not safetensors.
    #[test]
    fn trt_edgellm_pairs_only_with_a_gguf_vision_model() -> anyhow::Result<()> {
        let trt: Runtime = toml::from_str(
            r#"type = "trt_edgellm"
source = "relative_dir"
dir = "blobs"
flavor = "jetson-orin-cuda-13""#,
        )?;
        let vision: Model = toml::from_str(
            r#"type = "gguf_vision"
source = "huggingface"
org = "o"
repo_name = "r"
model = "m.gguf"
mmproj = "mm.gguf""#,
        )?;
        assert!(is_compatible(&vision, &trt));
        assert!(!is_compatible(&toml::from_str(GGUF_TEXT)?, &trt));
        assert!(!is_compatible(&torch_model()?, &trt));
        assert!(!is_compatible(&openvino_model()?, &trt));
        assert!(!types_compatible(
            ModelType::GgufVisionSpeculative,
            RuntimeType::TrtEdgellm
        ));
        Ok(())
    }

    /// ONNX GenAI pairs only with its runtime, both ways; OpenVINO is the
    /// plausible-looking wrong row (both are uv-served directory exports).
    #[test]
    fn onnx_genai_pairs_only_with_the_onnx_genai_runtime() -> anyhow::Result<()> {
        let model = onnx_genai_model()?;
        let onnx_genai = onnx_genai_runtime()?;
        assert!(is_compatible(&model, &onnx_genai));
        assert!(!is_compatible(&model, &openvino_runtime()?));

        // And the runtime is equally narrow: it loads ONNX, not OpenVINO IR.
        assert!(!is_compatible(&openvino_model()?, &onnx_genai));
        assert!(!is_compatible(&torch_model()?, &onnx_genai));
        Ok(())
    }

    /// A drafter pairs with the CLI and the iOS app, not the apk, which has
    /// neither a drafter nor a vision tower.
    #[test]
    fn speculative_vision_pairs_with_the_cli_and_ios_engines() -> anyhow::Result<()> {
        let model = Model::GgufVisionSpeculative(GgufVisionSpeculative {
            source: GgufVisionSpeculativeSource::HuggingFace {
                files: GgufVisionHfFiles {
                    repo: HfRepo::parse_org_repo("LiquidAI/LFM2.5-VL-3B-DSpark-4L-GGUF")?,
                    model: RepoSubpath::try_new("merged-f16.gguf".to_owned())?,
                    model_sha256: None,
                    mmproj: RepoSubpath::try_new("mmproj-f16.gguf".to_owned())?,
                    mmproj_sha256: None,
                },
                draft: None,
                draft_sha256: None,
            },
        });
        assert!(is_compatible(
            &model,
            &llamacpp("b10216", LlamaCppFlavor::MacosArm64)?
        ));
        assert!(is_compatible(
            &model,
            &Runtime::LlamacppIosPipette(LlamacppIosPipette {
                source: app_source("b1")?,
                flavor: LlamacppIosPipetteFlavor::IosArm64,
                private_apis: PrivateApis::default(),
            })
        ));
        assert!(!is_compatible(
            &model,
            &Runtime::LlamacppApkPipette(LlamacppApkPipette {
                source: app_source("b1")?,
                flavor: LlamacppApkPipetteFlavor::AndroidArm64V8,
            })
        ));
        Ok(())
    }

    /// The in-process iOS ONNX build loads ONNX GenAI only; `mlx_ios_pipette`,
    /// also a directory of weights on the device, is the plausible wrong row.
    #[test]
    fn onnx_genai_pairs_with_the_in_process_ios_runtime_too() -> anyhow::Result<()> {
        let model = onnx_genai_model()?;
        let ios = onnx_genai_ios_runtime()?;
        assert!(is_compatible(&model, &ios));

        let mlx_ios = mlx_ios_runtime()?;
        assert!(!is_compatible(&model, &mlx_ios));
        assert!(!is_compatible(&mlx_model()?, &ios));
        assert!(!is_compatible(&openvino_model()?, &ios));
        Ok(())
    }

    const MLX_SPECULATIVE: &str = r#"type = "mlx_speculative"
target = { source = "huggingface", org = "o", repo_name = "target" }
draft = { source = "huggingface", org = "o", repo_name = "draft" }"#;
    const TORCH_SPECULATIVE: &str = r#"type = "torch_speculative"
target = { source = "huggingface", org = "o", repo_name = "target" }
draft = { source = "huggingface", org = "o", repo_name = "draft" }"#;
    const MLX_MACOS: &str = r#"type = "mlx_macos_pipette"
version = "0.20.0"
flavor = "macos-arm64"
source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }"#;
    const MLX_IOS: &str = r#"type = "mlx_ios_pipette"
flavor = "ios-arm64"
[packages]
mlx_swift = { repository_url = "github.com/ml-explore/mlx-swift", repository_version = "0.25.6" }
mlx_swift_lm = { repository_url = "github.com/ml-explore/mlx-swift-examples", repository_version = "2.25.6" }
swift_transformers = { repository_url = "github.com/huggingface/swift-transformers", repository_version = "0.1.22" }"#;
    const DOCKER_SGLANG: &str = r#"type = "docker_sglang"
image_name = "lmsysorg/sglang"
image_tag = "v0.5.15-cu130"
flavor = "nvidia_gpu""#;
    const UV_SGLANG: &str = r#"type = "uv_sglang"
server_version = "0.4.0"
build = "cu121"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "sglang==0.4.0" }"#;
    const DOCKER_VLLM: &str = r#"type = "docker_vllm"
image_name = "vllm/vllm-openai"
image_tag = "v0.20.2"
flavor = "nvidia_gpu""#;
    const UV_VLLM: &str = r#"type = "uv_vllm"
server_version = "0.10.0"
build = "cu121"
python_version = "3.12"
source = { type = "pip_requirements_text", contents = "vllm==0.10.0" }"#;

    /// A drafter pair runs only where the engine has a drafter path: MLX on
    /// macOS, torch on SGLang.
    #[rstest::rstest]
    #[case::mlx_on_macos(MLX_SPECULATIVE, MLX_MACOS, true)]
    #[case::mlx_on_ios(MLX_SPECULATIVE, MLX_IOS, false)]
    #[case::torch_on_docker_sglang(TORCH_SPECULATIVE, DOCKER_SGLANG, true)]
    #[case::torch_on_uv_sglang(TORCH_SPECULATIVE, UV_SGLANG, true)]
    #[case::torch_on_docker_vllm(TORCH_SPECULATIVE, DOCKER_VLLM, false)]
    #[case::torch_on_uv_vllm(TORCH_SPECULATIVE, UV_VLLM, false)]
    fn speculative_pairs_run_only_on_engines_with_a_drafter(
        #[case] model: &str,
        #[case] runtime: &str,
        #[case] compatible: bool,
    ) -> anyhow::Result<()> {
        let model: Model = toml::from_str(model)?;
        let runtime: Runtime = toml::from_str(runtime)?;
        assert_eq!(is_compatible(&model, &runtime), compatible);
        Ok(())
    }

    #[test]
    fn apple_foundation_compatibility() -> anyhow::Result<()> {
        assert!(is_compatible(
            &Model::AppleFoundationText,
            &Runtime::AppleFoundation(Default::default())
        ));
        assert!(!is_compatible(
            &Model::AppleFoundationText,
            &llamacpp("b5000", LlamaCppFlavor::MacosArm64)?
        ));
        assert!(!is_compatible(
            &torch_model()?,
            &Runtime::AppleFoundation(Default::default())
        ));
        Ok(())
    }

    const GGUF_TEXT: &str = r#"type = "gguf_text"
source = "huggingface"
org = "o"
repo_name = "r"
path = "m.gguf""#;

    fn cli_runtime() -> anyhow::Result<Runtime> {
        llamacpp("b5000", LlamaCppFlavor::MacosArm64)
    }

    /// Mixed kinds pass as long as nothing dangles; each model keeps only the
    /// runtimes that load it.
    #[test]
    fn runnable_pairs_drop_the_incompatible_pairs() -> anyhow::Result<()> {
        let models = [mlx_model()?, torch_model()?];
        let runtimes: [Runtime; 3] = [
            toml::from_str(DOCKER_VLLM)?,
            toml::from_str(MLX_MACOS)?,
            toml::from_str(UV_SGLANG)?,
        ];
        let pairs = runnable_pairs_of(&models, &runtimes)?;
        let expected = [
            (&models[0], &runtimes[1]),
            (&models[1], &runtimes[0]),
            (&models[1], &runtimes[2]),
        ];
        assert_eq!(pairs.len(), expected.len(), "{pairs:?}");
        assert_eq!(
            pairs.into_iter().collect::<HashSet<_>>(),
            expected.into_iter().collect::<HashSet<_>>()
        );
        Ok(())
    }

    /// Repeated types collapse to one pair per (model type, runtime type) whose
    /// count is the pairs they stand for.
    #[test]
    fn runnable_type_pairs_count_the_runnable_pairs() -> anyhow::Result<()> {
        let models = [mlx_model()?, torch_model()?, mlx_model()?];
        let runtimes: [Runtime; 4] = [
            toml::from_str(DOCKER_VLLM)?,
            toml::from_str(MLX_MACOS)?,
            toml::from_str(UV_SGLANG)?,
            toml::from_str(DOCKER_VLLM)?,
        ];
        let type_pairs = runnable_type_pairs_of(&models, &runtimes)?
            .into_iter()
            .map(|(m, r, count)| (ModelType::of(m), RuntimeType::of(r), count))
            .collect::<Vec<_>>();
        let expected = [
            (ModelType::Mlx, RuntimeType::MlxMacosPipette, 2),
            (ModelType::Torch, RuntimeType::DockerVllm, 2),
            (ModelType::Torch, RuntimeType::UvSglang, 1),
        ];
        assert_eq!(type_pairs, expected);
        assert_eq!(
            type_pairs.iter().map(|&(_, _, count)| count).sum::<usize>(),
            runnable_pairs_of(&models, &runtimes)?.len()
        );
        Ok(())
    }

    /// The error carries the orphan, not a model that pairs.
    #[test]
    fn an_orphan_model_fails_the_variant() -> anyhow::Result<()> {
        let models = [torch_model()?, mlx_model()?];
        let runtimes: [Runtime; 1] = [toml::from_str(UV_VLLM)?];
        let result = runnable_pairs_of(&models, &runtimes);
        assert!(
            matches!(
                &result,
                Err(VariantError::ModelHasNoCompatibleRuntime(model))
                    if **model == models[1]
            ),
            "{result:?}"
        );
        Ok(())
    }

    #[test]
    fn an_orphan_runtime_fails_the_variant() -> anyhow::Result<()> {
        let models = [torch_model()?];
        let runtimes: [Runtime; 2] = [toml::from_str(UV_VLLM)?, toml::from_str(MLX_MACOS)?];
        let result = runnable_pairs_of(&models, &runtimes);
        assert!(
            matches!(
                &result,
                Err(VariantError::RuntimeHasNoCompatibleModel(runtime))
                    if **runtime == runtimes[1]
            ),
            "{result:?}"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::model(
        VariantError::ModelHasNoCompatibleRuntime(Box::new(Model::AppleFoundationText))
            .to_string(),
        "model apple/foundation-text has no compatible runtime in this variant"
    )]
    #[case::runtime(
        VariantError::RuntimeHasNoCompatibleModel(Box::new(Runtime::AppleFoundation(
            Default::default()
        )))
        .to_string(),
        "runtime apple_foundation has no compatible model in this variant"
    )]
    fn compatibility_errors_name_the_orphan(#[case] rendered: String, #[case] expected: &str) {
        assert_eq!(rendered, expected);
    }

    #[rstest::rstest]
    #[case::the_variants_list_wins(
        Some("eval_smoke"),
        Some("prefill_throughput_512"),
        "eval_smoke"
    )]
    #[case::the_plan_fallback(None, Some("prefill_throughput_512"), "prefill_throughput_512")]
    #[case::no_plan_list_needed(Some("eval_smoke"), None, "eval_smoke")]
    fn resolve_benchmarks_prefers_the_variants_list(
        #[case] variant: Option<&str>,
        #[case] plan: Option<&str>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let list = |id: Option<&str>| {
            id.map(|id| BenchmarkId::try_new(id.to_owned()).map(|id| vec![id]))
                .transpose()
        };
        let (variant, plan) = (list(variant)?, list(plan)?);
        let resolved = resolve_benchmarks(variant.as_deref(), plan.as_deref())?;
        assert_eq!(resolved, [BenchmarkId::try_new(expected.to_owned())?]);
        Ok(())
    }

    #[test]
    fn resolve_benchmarks_without_either_list_is_refused() {
        let error = resolve_benchmarks(None, None).err();
        assert_eq!(error, Some(VariantError::NoBenchmarks));
        assert_eq!(
            error.map(|e| e.to_string()).as_deref(),
            Some("benchmarks must be set on the variant or at the plan root")
        );
    }

    #[rstest::rstest]
    #[case::a_known_kind("decode_throughput_512_128", Some(BenchmarkType::DecodeThroughput))]
    #[case::an_unknown_kind("mystery_metric_1", None)]
    fn benchmark_type_of_reads_the_id(
        #[case] id: &str,
        #[case] expected: Option<BenchmarkType>,
    ) -> anyhow::Result<()> {
        let id = BenchmarkId::try_new(id.to_owned())?;
        assert_eq!(benchmark_type_of(&id), expected);
        Ok(())
    }

    /// A runtime or benchmark flag entry for gguf text on the llama.cpp CLI.
    fn cli_entry<T: serde::de::DeserializeOwned>(benchmark: &str, knob: &str) -> anyhow::Result<T> {
        Ok(toml::from_str(&format!(
            "benchmark_type = \"{benchmark}\"\nruntime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\n{knob}"
        ))?)
    }

    fn cli_entries<T: serde::de::DeserializeOwned>(
        wires: &[(&str, &str)],
    ) -> anyhow::Result<Vec<T>> {
        wires
            .iter()
            .map(|(benchmark, knob)| cli_entry(benchmark, knob))
            .collect()
    }

    fn run_spec(
        benchmark: &str,
        model: Model,
        runtime: Runtime,
        runtime_flags: Option<RuntimeFlags>,
    ) -> anyhow::Result<ClientRunSpec> {
        Ok(ClientRunSpec {
            benchmark: BenchmarkId::try_new(benchmark.to_owned())?,
            model,
            runtime,
            runtime_flags,
            model_flags: None,
            benchmark_flags: None,
        })
    }

    /// A gguf text spec on the llama.cpp CLI.
    fn cli_spec(
        benchmark: &str,
        runtime_flags: Option<RuntimeFlags>,
    ) -> anyhow::Result<ClientRunSpec> {
        run_spec(
            benchmark,
            toml::from_str(GGUF_TEXT)?,
            cli_runtime()?,
            runtime_flags,
        )
    }

    /// A runtime flag entry with no knobs set.
    fn runtime_entry(benchmark: &str, runtime: &str, model: &str) -> anyhow::Result<RuntimeFlags> {
        Ok(toml::from_str(&format!(
            "benchmark_type = \"{benchmark}\"\nruntime_type = \"{runtime}\"\nmodel_type = \"{model}\""
        ))?)
    }

    #[test]
    fn validate_agrees_with_is_compatible() -> anyhow::Result<()> {
        let models = [
            toml::from_str(GGUF_TEXT)?,
            openvino_model()?,
            mlx_model()?,
            torch_model()?,
            toml::from_str(MLX_SPECULATIVE)?,
            Model::AppleFoundationText,
        ];
        let runtimes = [
            cli_runtime()?,
            openvino_runtime()?,
            toml::from_str(MLX_MACOS)?,
            toml::from_str(MLX_IOS)?,
            toml::from_str(UV_SGLANG)?,
            Runtime::AppleFoundation(Default::default()),
        ];
        let mut compatible = 0;
        for model in &models {
            for runtime in &runtimes {
                let spec = run_spec("custom_probe", model.clone(), runtime.clone(), None)?;
                let expected = if is_compatible(model, runtime) {
                    compatible += 1;
                    Ok(())
                } else {
                    Err(RunSpecError::Incompatible {
                        model: ModelType::of(model),
                        runtime: RuntimeType::of(runtime),
                    })
                };
                assert_eq!(spec.validate(), expected, "{model} on {runtime}");
            }
        }
        // Both arms ran: every model here has a runtime, none has them all.
        assert!((models.len()..models.len() * runtimes.len()).contains(&compatible));
        Ok(())
    }

    #[test]
    fn an_incompatible_pair_names_its_types() -> anyhow::Result<()> {
        let spec = ClientRunSpec {
            runtime: openvino_runtime()?,
            ..cli_spec("prefill_throughput_256", None)?
        };
        assert_eq!(
            spec.validate().map_err(|e| e.to_string()),
            Err("runtime `uv_openvino` cannot load a `gguf_text` model".to_owned())
        );
        Ok(())
    }

    /// The pair is refused before its flags are read: flags for the model on
    /// another runtime would otherwise blame the flags.
    #[test]
    fn an_incompatible_pair_is_refused_ahead_of_its_flags() -> anyhow::Result<()> {
        let spec = ClientRunSpec {
            runtime: openvino_runtime()?,
            ..cli_spec("eval_mmlu", Some(cli_entry("eval", "ctx_size = 8192")?))?
        };
        assert!(
            matches!(spec.validate(), Err(RunSpecError::Incompatible { .. })),
            "{:?}",
            spec.validate()
        );
        Ok(())
    }

    #[test]
    fn a_refusal_never_carries_the_models_token() -> anyhow::Result<()> {
        use anyhow::Context;
        let model: Model = toml::from_str(&format!("{GGUF_TEXT}\nauth_token = \"hf_secret\""))?;
        let spec = run_spec("eval_mmlu", model, openvino_runtime()?, None)?;
        let message = spec
            .validate()
            .err()
            .context("must be refused")?
            .to_string();
        assert!(!message.contains("hf_secret"), "{message}");
        Ok(())
    }

    const RECORD_ENVS: &str = "envs = [\"HF_TOKEN=hf_env_secret\", \"GGML_HEXAGON_OPPOLL=1\"]";

    /// A gated GGUF eval spec, every flag group set.
    fn gated_gguf_spec() -> anyhow::Result<ClientRunSpec> {
        let model: Model = toml::from_str(&format!("{GGUF_TEXT}\nauth_token = \"hf_secret\""))?;
        Ok(ClientRunSpec {
            model_flags: Some(ModelFlags::EvalGgufText {
                enable_thinking: Some(false),
                reasoning_effort: None,
            }),
            benchmark_flags: Some(cli_entry("eval", "http_timeout_seconds = 600")?),
            ..run_spec(
                "eval_mmlu",
                model,
                cli_runtime()?,
                Some(cli_entry("eval", RECORD_ENVS)?),
            )?
        })
    }

    /// A speculative pair on SGLang whose target and drafter are both gated.
    fn gated_draft_spec() -> anyhow::Result<ClientRunSpec> {
        let model: Model = toml::from_str(
            r#"type = "torch_speculative"
target = { source = "huggingface", org = "o", repo_name = "target", auth_token = "hf_secret" }
draft = { source = "huggingface", org = "o", repo_name = "draft", auth_token = "hf_secret" }"#,
        )?;
        let flags = toml::from_str(&format!(
            "benchmark_type = \"vl_spec_decode\"\nruntime_type = \"uv_sglang\"\nmodel_type = \"torch_speculative\"\n{RECORD_ENVS}"
        ))?;
        run_spec(
            "vl_spec_decode_corpus",
            model,
            toml::from_str(UV_SGLANG)?,
            Some(flags),
        )
    }

    /// A record keeps no token, the drafter's included, and no env value but
    /// `GGML_*`'s; the rest of the spec is as it was.
    #[rstest::rstest]
    #[case::gguf(gated_gguf_spec())]
    #[case::speculative_draft(gated_draft_spec())]
    fn a_spec_as_a_record_carries_no_token_and_only_ggml_env_values(
        #[case] spec: anyhow::Result<ClientRunSpec>,
    ) -> anyhow::Result<()> {
        let spec = spec?;
        anyhow::ensure!(spec.model.auth_token().is_some(), "the fixture is gated");
        let record = spec.for_record();

        anyhow::ensure!(record.model.auth_token().is_none(), "{:?}", record.model);
        let envs: Vec<String> = record
            .runtime_flags
            .iter()
            .flat_map(|flags| flags.envs().iter().map(ToString::to_string))
            .collect();
        let [hidden, kept] = envs.as_slice() else {
            anyhow::bail!("two forwards: {envs:?}");
        };
        assert_eq!(hidden, "HF_TOKEN");
        assert_eq!(kept, "GGML_HEXAGON_OPPOLL=1");
        let wire = serde_json::to_string(&record)?;
        anyhow::ensure!(!wire.contains("secret"), "{wire}");

        let untouched = ClientRunSpec {
            model: spec.model.without_auth_token(),
            runtime_flags: record.runtime_flags.clone(),
            ..spec
        };
        assert_eq!(record, untouched);
        Ok(())
    }

    /// Only the runtime flags' runtime and model are checked: their benchmark,
    /// the spec's, and the other flag groups are the client's to resolve.
    #[rstest::rstest]
    #[case::no_flags(cli_spec("prefill_throughput_256", None))]
    #[case::the_benchmarks_own_flags(cli_entry("eval", "ctx_size = 8192").and_then(|flags| cli_spec("eval_mmlu", Some(flags))))]
    #[case::flags_of_another_benchmark(cli_entry("eval", "ctx_size = 8192").and_then(|flags| cli_spec("prefill_throughput_256", Some(flags))))]
    #[case::an_unknown_benchmark(cli_entry("eval", "ctx_size = 8192").and_then(|flags| cli_spec("custom_probe", Some(flags))))]
    #[case::model_and_benchmark_flags_of_another_cell(cli_spec("prefill_throughput_256", None).and_then(|spec| Ok(ClientRunSpec {
        model_flags: Some(ModelFlags::EvalTorch { enable_thinking: None, reasoning_effort: None }),
        benchmark_flags: Some(cli_entry("eval", "http_timeout_seconds = 600")?),
        ..spec
    })))]
    #[case::mlx_on_macos(runtime_entry("vl_spec_decode", "mlx_macos_pipette", "mlx")
        .and_then(|flags| run_spec("vl_spec_decode", mlx_model()?, toml::from_str(MLX_MACOS)?, Some(flags))))]
    #[case::speculative_mlx_on_macos(runtime_entry("vl_spec_decode", "mlx_macos_pipette", "mlx_speculative")
        .and_then(|flags| run_spec("vl_spec_decode", toml::from_str(MLX_SPECULATIVE)?, toml::from_str(MLX_MACOS)?, Some(flags))))]
    #[case::openvino(runtime_entry("prefill_throughput", "uv_openvino", "openvino")
        .and_then(|flags| run_spec("prefill_throughput_256", openvino_model()?, openvino_runtime()?, Some(flags))))]
    fn validate_accepts_the_pairs_runtime_flags(
        #[case] spec: anyhow::Result<ClientRunSpec>,
    ) -> anyhow::Result<()> {
        Ok(spec?.validate()?)
    }

    #[rstest::rstest]
    #[case::another_runtime(
        runtime_entry("prefill_throughput", "uv_openvino", "openvino").and_then(|flags| cli_spec("prefill_throughput_256", Some(flags))),
        "`runtime_flags` are for `uv_openvino` × `openvino`, and the spec is \
         `llamacpp_cli_stock_tools` × `gguf_text`"
    )]
    #[case::another_model_on_the_runtime(
        runtime_entry("vl_throughput", "llamacpp_cli_stock_tools", "gguf_vision").and_then(|flags| cli_spec("vl_throughput_256", Some(flags))),
        "`runtime_flags` are for `llamacpp_cli_stock_tools` × `gguf_vision`, and the spec is \
         `llamacpp_cli_stock_tools` × `gguf_text`"
    )]
    #[case::plain_mlx_flags_on_a_speculative_model(
        runtime_entry("vl_spec_decode", "mlx_macos_pipette", "mlx").and_then(|flags| run_spec(
            "vl_spec_decode", toml::from_str(MLX_SPECULATIVE)?, toml::from_str(MLX_MACOS)?, Some(flags),
        )),
        "`runtime_flags` are for `mlx_macos_pipette` × `mlx`, and the spec is \
         `mlx_macos_pipette` × `mlx_speculative`"
    )]
    fn validate_refuses_runtime_flags_for_another_pair(
        #[case] spec: anyhow::Result<ClientRunSpec>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(
            spec?.validate().map_err(|e| e.to_string()),
            Err(expected.to_owned())
        );
        Ok(())
    }

    /// Entry 1 is the cell's; entry 2 repeats its key, pinning that the first
    /// match wins on an unvalidated list.
    #[rstest::rstest]
    #[case::no_benchmark_type(None, false)]
    #[case::the_cells_entry(Some(BenchmarkType::Eval), true)]
    #[case::no_entry_for_the_cell(Some(BenchmarkType::DecodeThroughput), false)]
    fn flag_resolvers_pick_the_cells_entry(
        #[case] benchmark: Option<BenchmarkType>,
        #[case] resolves: bool,
    ) -> anyhow::Result<()> {
        let (model, runtime) = (toml::from_str::<Model>(GGUF_TEXT)?, cli_runtime()?);
        let runtime_flags: Vec<RuntimeFlags> = cli_entries(&[
            ("prefill_throughput", "threads = 4"),
            ("eval", "ctx_size = 8192"),
            ("eval", "ctx_size = 4096"),
        ])?;
        let model_flags = [
            ModelFlags::EvalTorch {
                enable_thinking: Some(true),
                reasoning_effort: None,
            },
            ModelFlags::EvalGgufText {
                enable_thinking: Some(true),
                reasoning_effort: None,
            },
            ModelFlags::EvalGgufText {
                enable_thinking: Some(false),
                reasoning_effort: None,
            },
        ];
        let benchmark_flags: Vec<BenchmarkFlags> = cli_entries(&[
            ("prefill_throughput", "readiness = { max_wait_secs = 60 }"),
            ("eval", "http_timeout_seconds = 600"),
            ("eval", "http_timeout_seconds = 900"),
        ])?;
        assert_eq!(
            resolve_runtime_flags(&runtime_flags, benchmark, &runtime, &model),
            resolves.then(|| runtime_flags[1].clone())
        );
        assert_eq!(
            resolve_model_flags(&model_flags, benchmark, &model),
            resolves.then(|| model_flags[1].clone())
        );
        assert_eq!(
            resolve_benchmark_flags(&benchmark_flags, benchmark, &runtime, &model),
            resolves.then(|| benchmark_flags[1].clone())
        );
        Ok(())
    }

    /// A gguf text variant on the llama.cpp CLI, running prefill and eval.
    fn validate(
        runtime_flags: &[(&str, &str)],
        model_flags: &[ModelFlags],
        benchmark_flags: &[(&str, &str)],
    ) -> anyhow::Result<()> {
        let (model, runtime) = (toml::from_str::<Model>(GGUF_TEXT)?, cli_runtime()?);
        Ok(validate_variant_flag_cells(
            &[(&model, &runtime)],
            &[BenchmarkType::PrefillThroughput, BenchmarkType::Eval],
            &cli_entries::<RuntimeFlags>(runtime_flags)?,
            model_flags,
            &cli_entries::<BenchmarkFlags>(benchmark_flags)?,
        )?)
    }

    #[test]
    fn a_flag_set_on_the_variants_cells_passes() -> anyhow::Result<()> {
        validate(
            &[
                ("prefill_throughput", "threads = 4"),
                ("eval", "ctx_size = 8192"),
            ],
            &[ModelFlags::EvalGgufText {
                enable_thinking: Some(true),
                reasoning_effort: None,
            }],
            &[
                ("prefill_throughput", "readiness = { max_wait_secs = 60 }"),
                ("eval", "http_timeout_seconds = 600"),
            ],
        )
    }

    #[rstest::rstest]
    #[case::duplicate_runtime_flags(
        &[("eval", "ctx_size = 8192"), ("eval", "ctx_size = 4096")],
        &[],
        &[],
        "duplicate runtime_flags entry for the same cell triple"
    )]
    #[case::duplicate_model_flags(
        &[],
        &[
            ModelFlags::EvalGgufText { enable_thinking: Some(true), reasoning_effort: None },
            ModelFlags::EvalGgufText { enable_thinking: Some(false), reasoning_effort: None },
        ],
        &[],
        "duplicate model_flags entry for the same cell"
    )]
    #[case::duplicate_benchmark_flags(
        &[],
        &[],
        &[("eval", "http_timeout_seconds = 600"), ("eval", "http_timeout_seconds = 900")],
        "duplicate benchmark_flags entry for the same cell"
    )]
    #[case::runtime_flags_on_no_cell(
        &[("decode_throughput", "threads = 4")],
        &[],
        &[],
        "a runtime_flags entry matches no cell in the variant \
         (no benchmark\u{d7}runtime\u{d7}model triple satisfies it)"
    )]
    #[case::model_flags_on_no_cell(
        &[],
        &[ModelFlags::EvalTorch { enable_thinking: Some(true), reasoning_effort: None }],
        &[],
        "a model_flags entry matches no cell in the variant \
         (no eval benchmark\u{d7}model pair satisfies it)"
    )]
    #[case::benchmark_flags_on_no_cell(
        &[],
        &[],
        &[("decode_throughput", "")],
        "a benchmark_flags entry matches no cell in the variant \
         (no benchmark\u{d7}runtime\u{d7}model triple satisfies it)"
    )]
    fn validate_variant_flag_cells_refusals(
        #[case] runtime_flags: &[(&str, &str)],
        #[case] model_flags: &[ModelFlags],
        #[case] benchmark_flags: &[(&str, &str)],
        #[case] message: &str,
    ) {
        let error = validate(runtime_flags, model_flags, benchmark_flags)
            .err()
            .map(|e| e.to_string());
        assert_eq!(error.as_deref(), Some(message));
    }

    fn client_run_spec() -> anyhow::Result<ClientRunSpec> {
        Ok(ClientRunSpec {
            benchmark: BenchmarkId::try_new("eval_smoke".to_owned())?,
            model: toml::from_str(GGUF_TEXT)?,
            runtime: cli_runtime()?,
            runtime_flags: None,
            model_flags: None,
            benchmark_flags: None,
        })
    }

    /// A job body may omit an unset flag group, yet serialization emits each
    /// key as `null`, so stored state files stay byte-identical.
    #[test]
    fn client_run_spec_flag_groups_may_be_omitted() -> anyhow::Result<()> {
        let spec = client_run_spec()?;
        let body = serde_json::json!({
            "benchmark": "eval_smoke",
            "model": serde_json::to_value(&spec.model)?,
            "runtime": serde_json::to_value(&spec.runtime)?,
        });
        assert_eq!(serde_json::from_value::<ClientRunSpec>(body)?, spec);

        let emitted = serde_json::to_value(&spec)?;
        for key in ["runtime_flags", "model_flags", "benchmark_flags"] {
            anyhow::ensure!(
                emitted.get(key) == Some(&serde_json::Value::Null),
                "{key} is not emitted as null: {emitted}"
            );
        }
        Ok(())
    }

    #[test]
    fn client_run_spec_round_trips_every_flag_group() -> anyhow::Result<()> {
        let spec = ClientRunSpec {
            runtime_flags: Some(cli_entry("eval", "ctx_size = 8192")?),
            model_flags: Some(ModelFlags::EvalGgufText {
                enable_thinking: Some(false),
                reasoning_effort: None,
            }),
            benchmark_flags: Some(cli_entry("eval", "http_timeout_seconds = 600")?),
            ..client_run_spec()?
        };
        let json = serde_json::to_string(&spec)?;
        assert_eq!(serde_json::from_str::<ClientRunSpec>(&json)?, spec);
        Ok(())
    }
}
