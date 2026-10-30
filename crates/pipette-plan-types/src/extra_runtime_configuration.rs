//! What a client adds to every process of a runtime type, beyond what the
//! plan sets: the `--config` file's `[[runtimes]]` entries. A host fact, never
//! a cell one: none of it enters the cell key, the request or `runtime_flags`.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ClientRunSpec, EnvForward, Runtime, RuntimeType};

/// One `[[runtimes]]` entry as the file spells it, flat, and the wire form
/// [`ExtraRuntimeConfiguration`] serializes through.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraRuntimeConfigurationRef {
    pub runtime_type: RuntimeType,
    /// Environment for every process a run of this runtime spawns. Literal
    /// `NAME=VALUE` only: the client's own environment is the host's, so the
    /// inherit form would say nothing.
    #[serde(default)]
    pub envs: Vec<EnvForward>,
}

/// What the extra runtime configuration gives the processes one engine spawns.
/// Engines take it through this trait, never its fields, so a knob that is not
/// environment can be added without touching a spawn site.
pub trait EngineExtraRuntimeConfiguration {
    /// The environment for every process the engine spawns, applied first so a
    /// host file cannot clobber a variable the launch sets.
    fn envs(&self) -> &[EnvForward];
}

/// The configuration an engine's entrypoint accepts, so handing an engine
/// another's entry does not compile. [`TorchOaiExtraRuntimeConfiguration`] is
/// named for its engine, which spawns four runtime types; the
/// [`ExtraRuntimeConfiguration`] enum stays keyed by runtime type, as the file
/// and the record are.
///
/// Identical today, but separate so one engine can take a knob the others do not.
macro_rules! engine_client_configurations {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        pub struct $name {
            envs: Vec<EnvForward>,
        }

        impl EngineExtraRuntimeConfiguration for $name {
            fn envs(&self) -> &[EnvForward] {
                &self.envs
            }
        }

        impl From<Vec<EnvForward>> for $name {
            fn from(envs: Vec<EnvForward>) -> Self {
                Self { envs }
            }
        }
    )+};
}

engine_client_configurations! {
    /// Applied to every llama.cpp tool a run spawns.
    LlamacppCliStockToolsExtraRuntimeConfiguration,
    /// Applied to the MLX server process.
    MlxMacosPipetteExtraRuntimeConfiguration,
    /// Applied to the vLLM/SGLang server, under docker or uv alike.
    TorchOaiExtraRuntimeConfiguration,
    /// Applied to every OpenVINO driver and tokenizer process.
    UvOpenvinoExtraRuntimeConfiguration,
    /// Applied to every ONNX GenAI driver and tokenizer process.
    UvOnnxGenaiExtraRuntimeConfiguration,
    /// Applied to the OVMS server process.
    OvmsExtraRuntimeConfiguration,
    /// Applied to the TensorRT Edge-LLM engine build and server processes.
    TrtEdgellmExtraRuntimeConfiguration,
}

/// Why an [`ExtraRuntimeConfiguration`] refused: an entry the file cannot mean,
/// a cell it would contradict, or an engine it is not for. The file is
/// operator-authored, so which way it was wrong is worth keeping.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtraRuntimeConfigurationError {
    /// The inherit form (`NAME` with no value) in a client file.
    #[error(
        "[[runtimes]] {runtime_type}: env `{entry}` has no value; the client's own \
         environment is the host's, so spell the value"
    )]
    InheritFormNotAllowed {
        runtime_type: RuntimeType,
        entry: EnvForward,
    },
    /// A runtime type that runs on a phone or a tablet, not through a client.
    #[error(
        "[[runtimes]] {runtime_type} runs on a device, not through this client; \
         its processes are not this client's to configure"
    )]
    OnDeviceRuntime { runtime_type: RuntimeType },
    /// A cell forwards a variable this client's entry sets. Refused rather than
    /// run with one side silently overriding the other.
    #[error(
        "the cell's `{entry}` env forward names a variable the client \
         configuration sets for {runtime_type}; drop one of the two"
    )]
    CellEnvConflict {
        runtime_type: RuntimeType,
        entry: EnvForward,
    },
    /// An [`ExtraRuntimeConfiguration::as_*`](ExtraRuntimeConfiguration)
    /// accessor was called by an engine the entry is not for.
    #[error("the extra runtime configuration for {got} was handed to the {expected} engine")]
    WrongEngine {
        expected: &'static str,
        got: RuntimeType,
    },
}

/// How a client runs one runtime type; a variant exists only for the types a
/// client spawns. Serializes through [`ExtraRuntimeConfigurationRef`], so a
/// configuration reads back as the entry an operator wrote.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    try_from = "ExtraRuntimeConfigurationRef",
    into = "ExtraRuntimeConfigurationRef"
)]
pub enum ExtraRuntimeConfiguration {
    LlamacppCliStockTools(LlamacppCliStockToolsExtraRuntimeConfiguration),
    MlxMacosPipette(MlxMacosPipetteExtraRuntimeConfiguration),
    DockerVllm(TorchOaiExtraRuntimeConfiguration),
    DockerSglang(TorchOaiExtraRuntimeConfiguration),
    UvVllm(TorchOaiExtraRuntimeConfiguration),
    UvSglang(TorchOaiExtraRuntimeConfiguration),
    UvOpenvino(UvOpenvinoExtraRuntimeConfiguration),
    UvOnnxGenai(UvOnnxGenaiExtraRuntimeConfiguration),
    Ovms(OvmsExtraRuntimeConfiguration),
    TrtEdgellm(TrtEdgellmExtraRuntimeConfiguration),
}

impl TryFrom<ExtraRuntimeConfigurationRef> for ExtraRuntimeConfiguration {
    type Error = ExtraRuntimeConfigurationError;

    fn try_from(r: ExtraRuntimeConfigurationRef) -> Result<Self, ExtraRuntimeConfigurationError> {
        use RuntimeType as R;
        let runtime_type = r.runtime_type;
        if let Some(entry) = r.envs.iter().find(|e| e.value().is_none()) {
            return Err(ExtraRuntimeConfigurationError::InheritFormNotAllowed {
                runtime_type,
                entry: entry.clone(),
            });
        }
        let envs = r.envs;
        Ok(match runtime_type {
            R::LlamacppCliStockTools => Self::LlamacppCliStockTools(envs.into()),
            R::MlxMacosPipette => Self::MlxMacosPipette(envs.into()),
            R::DockerVllm => Self::DockerVllm(envs.into()),
            R::DockerSglang => Self::DockerSglang(envs.into()),
            R::UvVllm => Self::UvVllm(envs.into()),
            R::UvSglang => Self::UvSglang(envs.into()),
            R::UvOpenvino => Self::UvOpenvino(envs.into()),
            R::UvOnnxGenai => Self::UvOnnxGenai(envs.into()),
            R::Ovms => Self::Ovms(envs.into()),
            R::TrtEdgellm => Self::TrtEdgellm(envs.into()),
            R::LlamacppApkPipette
            | R::LlamacppIosPipette
            | R::MlxIosPipette
            | R::OnnxGenaiIosPipette
            | R::AppleFoundation => {
                return Err(ExtraRuntimeConfigurationError::OnDeviceRuntime { runtime_type });
            }
        })
    }
}

impl From<ExtraRuntimeConfiguration> for ExtraRuntimeConfigurationRef {
    fn from(c: ExtraRuntimeConfiguration) -> Self {
        Self {
            runtime_type: c.runtime_type(),
            envs: c.envs().to_vec(),
        }
    }
}

impl ExtraRuntimeConfiguration {
    pub fn runtime_type(&self) -> RuntimeType {
        match self {
            Self::LlamacppCliStockTools(_) => RuntimeType::LlamacppCliStockTools,
            Self::MlxMacosPipette(_) => RuntimeType::MlxMacosPipette,
            Self::DockerVllm(_) => RuntimeType::DockerVllm,
            Self::DockerSglang(_) => RuntimeType::DockerSglang,
            Self::UvVllm(_) => RuntimeType::UvVllm,
            Self::UvSglang(_) => RuntimeType::UvSglang,
            Self::UvOpenvino(_) => RuntimeType::UvOpenvino,
            Self::UvOnnxGenai(_) => RuntimeType::UvOnnxGenai,
            Self::Ovms(_) => RuntimeType::Ovms,
            Self::TrtEdgellm(_) => RuntimeType::TrtEdgellm,
        }
    }

    /// Every entry, whichever engine the configuration is for. An engine reads
    /// its own configuration instead, through [`EngineExtraRuntimeConfiguration`].
    pub fn envs(&self) -> &[EnvForward] {
        match self {
            Self::LlamacppCliStockTools(c) => c.envs(),
            Self::MlxMacosPipette(c) => c.envs(),
            Self::DockerVllm(c) | Self::DockerSglang(c) | Self::UvVllm(c) | Self::UvSglang(c) => {
                c.envs()
            }
            Self::UvOpenvino(c) => c.envs(),
            Self::UvOnnxGenai(c) => c.envs(),
            Self::Ovms(c) => c.envs(),
            Self::TrtEdgellm(c) => c.envs(),
        }
    }

    fn unexpected(&self, expected: &'static str) -> ExtraRuntimeConfigurationError {
        ExtraRuntimeConfigurationError::WrongEngine {
            expected,
            got: self.runtime_type(),
        }
    }

    /// This entry as the llama.cpp engine's configuration, or an error naming
    /// both sides: a mis-wired dispatch fails rather than recording a
    /// configuration the run did not have.
    pub fn as_llamacpp(
        &self,
    ) -> Result<&LlamacppCliStockToolsExtraRuntimeConfiguration, ExtraRuntimeConfigurationError>
    {
        let Self::LlamacppCliStockTools(c) = self else {
            return Err(self.unexpected("llama.cpp"));
        };
        Ok(c)
    }

    /// This entry as the MLX engine's configuration; see [`Self::as_llamacpp`].
    pub fn as_mlx(
        &self,
    ) -> Result<&MlxMacosPipetteExtraRuntimeConfiguration, ExtraRuntimeConfigurationError> {
        let Self::MlxMacosPipette(c) = self else {
            return Err(self.unexpected("MLX"));
        };
        Ok(c)
    }

    /// This entry as the torch-oai engine's configuration — the one engine with
    /// four runtime types, so four variants answer; see [`Self::as_llamacpp`].
    pub fn as_torch_oai(
        &self,
    ) -> Result<&TorchOaiExtraRuntimeConfiguration, ExtraRuntimeConfigurationError> {
        let (Self::DockerVllm(c) | Self::DockerSglang(c) | Self::UvVllm(c) | Self::UvSglang(c)) =
            self
        else {
            return Err(self.unexpected("torch-oai"));
        };
        Ok(c)
    }

    /// This entry as the OpenVINO engine's configuration; see [`Self::as_llamacpp`].
    pub fn as_openvino(
        &self,
    ) -> Result<&UvOpenvinoExtraRuntimeConfiguration, ExtraRuntimeConfigurationError> {
        let Self::UvOpenvino(c) = self else {
            return Err(self.unexpected("OpenVINO"));
        };
        Ok(c)
    }

    /// This entry as the ONNX GenAI engine's configuration; see [`Self::as_llamacpp`].
    pub fn as_onnx_genai(
        &self,
    ) -> Result<&UvOnnxGenaiExtraRuntimeConfiguration, ExtraRuntimeConfigurationError> {
        let Self::UvOnnxGenai(c) = self else {
            return Err(self.unexpected("ONNX GenAI"));
        };
        Ok(c)
    }

    /// This entry as the OVMS engine's configuration; see [`Self::as_llamacpp`].
    pub fn as_ovms(
        &self,
    ) -> Result<&OvmsExtraRuntimeConfiguration, ExtraRuntimeConfigurationError> {
        let Self::Ovms(c) = self else {
            return Err(self.unexpected("OVMS"));
        };
        Ok(c)
    }

    /// This entry as the TensorRT Edge-LLM engine's configuration; see
    /// [`Self::as_llamacpp`].
    pub fn as_trt_edgellm(
        &self,
    ) -> Result<&TrtEdgellmExtraRuntimeConfiguration, ExtraRuntimeConfigurationError> {
        let Self::TrtEdgellm(c) = self else {
            return Err(self.unexpected("TensorRT Edge-LLM"));
        };
        Ok(c)
    }

    /// What a result records about the configuration its run was made under:
    /// this entry, with `runtime_type` dropped and env values stripped to
    /// names outside `GGML_*`.
    ///
    /// Recorded because it is a measurement condition
    /// (`GGML_CUDA_ENABLE_UNIFIED_MEMORY` moves the numbers) nothing else in the
    /// payload shows; beside `runtime_flags`, never inside, as it is not part of
    /// the cell. `runtime_type` goes because `runtime_descriptor` already names
    /// the runtime. Values are redacted as a cell's forwards are
    /// ([`EnvForward::for_record`]).
    pub fn submission_value(&self) -> serde_json::Value {
        let envs: Vec<EnvForward> = self.envs().iter().map(EnvForward::for_record).collect();
        // Serializing cannot fail; fall closed on an empty object rather than
        // dropping the record silently.
        let mut value = serde_json::to_value(ExtraRuntimeConfigurationRef {
            runtime_type: self.runtime_type(),
            envs,
        })
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(fields) = value.as_object_mut() {
            fields.remove("runtime_type");
        }
        value
    }

    /// Refuse a cell whose `envs` name a variable this sets, rather than run
    /// it with one side silently overriding the other.
    pub fn admit(&self, spec: &ClientRunSpec) -> Result<(), ExtraRuntimeConfigurationError> {
        let Some(flags) = spec.runtime_flags.as_ref() else {
            return Ok(());
        };
        match flags
            .envs()
            .iter()
            .find(|e| self.envs().iter().any(|mine| mine.name() == e.name()))
        {
            Some(entry) => Err(ExtraRuntimeConfigurationError::CellEnvConflict {
                runtime_type: self.runtime_type(),
                entry: entry.clone(),
            }),
            None => Ok(()),
        }
    }
}

impl fmt::Display for ExtraRuntimeConfiguration {
    /// For the startup log: the env *names*. A value may be a token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.envs().iter().map(EnvForward::name).collect();
        write!(f, "{}: envs=[{}]", self.runtime_type(), names.join(", "))
    }
}

/// Why the `[[runtimes]]` entries could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ExtraRuntimeConfigurationsError {
    /// Two entries for one runtime type: which one applies would be file order.
    #[error("[[runtimes]] {runtime_type} appears twice; one entry per runtime type")]
    DuplicateRuntimeType { runtime_type: RuntimeType },
    #[error(transparent)]
    Entry(#[from] ExtraRuntimeConfigurationError),
}

/// Every [`ExtraRuntimeConfiguration`] this client runs under, at most one per
/// runtime type. Read-only once built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraRuntimeConfigurations {
    by_type: HashMap<RuntimeType, ExtraRuntimeConfiguration>,
}

impl ExtraRuntimeConfigurations {
    /// Resolve a file's `[[runtimes]]` entries, as the client read them. The
    /// file itself is the client's to read.
    pub fn new(
        runtimes: Vec<ExtraRuntimeConfigurationRef>,
    ) -> Result<Self, ExtraRuntimeConfigurationsError> {
        runtimes
            .into_iter()
            .try_fold(HashMap::new(), |mut by_type, entry| {
                let runtime_type = entry.runtime_type;
                let config = ExtraRuntimeConfiguration::try_from(entry)?;
                match by_type.insert(runtime_type, config) {
                    None => Ok(by_type),
                    Some(_) => {
                        Err(ExtraRuntimeConfigurationsError::DuplicateRuntimeType { runtime_type })
                    }
                }
            })
            .map(|by_type| Self { by_type })
    }

    #[cfg(test)]
    fn parse(text: &str) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            #[serde(default)]
            runtimes: Vec<ExtraRuntimeConfigurationRef>,
        }
        let file: File = toml::from_str(text)?;
        Ok(Self::new(file.runtimes)?)
    }

    /// The configuration for `runtime`, if the client has one.
    pub fn for_runtime(&self, runtime: &Runtime) -> Option<&ExtraRuntimeConfiguration> {
        self.by_type.get(&RuntimeType::of(runtime))
    }

    /// Refuse a cell the configuration for its runtime would contradict
    /// ([`ExtraRuntimeConfiguration::admit`]).
    pub fn admit(&self, spec: &ClientRunSpec) -> Result<(), ExtraRuntimeConfigurationError> {
        self.for_runtime(&spec.runtime)
            .map_or(Ok(()), |config| config.admit(spec))
    }
}

impl fmt::Display for ExtraRuntimeConfigurations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut entries: Vec<&ExtraRuntimeConfiguration> = self.by_type.values().collect();
        entries.sort_by_key(|c| c.runtime_type().to_string());
        if entries.is_empty() {
            return write!(f, "(none)");
        }
        let rendered: Vec<String> = entries.iter().map(ToString::to_string).collect();
        write!(f, "{}", rendered.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{BenchmarkId, Model, RuntimeFlags};

    fn llamacpp() -> anyhow::Result<Runtime> {
        Ok(serde_json::from_value(serde_json::json!({
            "type": "llamacpp_cli_stock_tools", "source": "github_release",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "version": "b5000", "flavor": "linux-x64-cpu"
        }))?)
    }

    fn docker_vllm() -> anyhow::Result<Runtime> {
        Ok(serde_json::from_value(serde_json::json!({
            "type": "docker_vllm", "image_name": "vllm/vllm-openai",
            "image_tag": "v0.21.0", "flavor": "nvidia_gpu"
        }))?)
    }

    fn mlx() -> anyhow::Result<Runtime> {
        Ok(serde_json::from_value(serde_json::json!({
            "type": "mlx_macos_pipette", "version": "0.31.3", "flavor": "macos-arm64",
            "source": { "type": "pip_requirements_text", "contents": "mlx-lm==0.31.3\n" }
        }))?)
    }

    fn envs(entries: &[&str]) -> anyhow::Result<Vec<EnvForward>> {
        entries
            .iter()
            .map(|e| Ok(EnvForward::try_new((*e).to_owned())?))
            .collect()
    }

    fn spelled(entries: &[EnvForward]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    /// An eval cell on `runtime` forwarding `cell_envs`.
    fn spec(runtime: Runtime, cell_envs: &[&str]) -> anyhow::Result<ClientRunSpec> {
        let model: Model = serde_json::from_value(serde_json::json!({
            "type": "gguf_text", "source": "huggingface", "org": "o", "repo_name": "r",
            "path": "m-Q4_0.gguf"
        }))?;
        let runtime_flags = (!cell_envs.is_empty())
            .then(|| {
                anyhow::Ok(RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                    crate::EvalLlamacppCliStockToolsGgufText {
                        threads: None,
                        number_gpu_layers: None,
                        mmap: None,
                        load_mode: None,
                        flash_attention: None,
                        ctx_size: None,
                        no_cache: None,
                        parallel: None,
                        envs: envs(cell_envs)?,
                        raw: vec![],
                    },
                ))
            })
            .transpose()?;
        Ok(ClientRunSpec {
            benchmark: BenchmarkId::try_new("eval_smoke".to_owned())?,
            model,
            runtime,
            runtime_flags,
            model_flags: None,
            benchmark_flags: None,
        })
    }

    const FULL: &str = r#"
[[runtimes]]
runtime_type = "llamacpp_cli_stock_tools"
envs = ["CUDA_DEVICE_ORDER=PCI_BUS_ID", "CUDA_VISIBLE_DEVICES=1"]

[[runtimes]]
runtime_type = "docker_vllm"
envs = ["VLLM_WORKER_MULTIPROC_METHOD=spawn"]
"#;

    #[test]
    fn the_file_becomes_one_variant_per_runtime_type() -> anyhow::Result<()> {
        let config = ExtraRuntimeConfigurations::parse(FULL)?;
        assert_eq!(
            config.for_runtime(&llamacpp()?),
            Some(&ExtraRuntimeConfiguration::LlamacppCliStockTools(
                envs(&["CUDA_DEVICE_ORDER=PCI_BUS_ID", "CUDA_VISIBLE_DEVICES=1"])?.into()
            ))
        );
        assert_eq!(
            config.for_runtime(&docker_vllm()?),
            Some(&ExtraRuntimeConfiguration::DockerVllm(
                envs(&["VLLM_WORKER_MULTIPROC_METHOD=spawn"])?.into()
            ))
        );
        assert!(config.for_runtime(&mlx()?).is_none());
        assert_eq!(
            ExtraRuntimeConfigurations::parse("")?,
            ExtraRuntimeConfigurations::default()
        );
        Ok(())
    }

    /// A typo cannot silently apply to nothing: each malformation is refused
    /// with the offending spelling in the message.
    #[rstest]
    #[case::unknown_table("[placement]\ngpu = 1\n", "placement")]
    #[case::unknown_key("[[runtimes]]\nruntime_type = \"docker_vllm\"\ngpu = 1\n", "gpu")]
    #[case::unknown_runtime_type("[[runtimes]]\nruntime_type = \"vllm\"\n", "vllm")]
    #[case::bad_env_name(
        "[[runtimes]]\nruntime_type = \"docker_vllm\"\nenvs = [\"NOT VALID=1\"]\n",
        "NOT VALID"
    )]
    // The refusals this module owns, spelled in full.
    #[case::on_device_runtime(
        "[[runtimes]]\nruntime_type = \"llamacpp_apk_pipette\"\n",
        "[[runtimes]] llamacpp_apk_pipette runs on a device, not through this client; \
         its processes are not this client's to configure"
    )]
    #[case::inherit_form(
        "[[runtimes]]\nruntime_type = \"docker_vllm\"\nenvs = [\"A=1\", \"HF_TOKEN\"]\n",
        "[[runtimes]] docker_vllm: env `HF_TOKEN` has no value; the client's own \
         environment is the host's, so spell the value"
    )]
    #[case::duplicate_type(
        "[[runtimes]]\nruntime_type = \"docker_vllm\"\n[[runtimes]]\nruntime_type = \"docker_vllm\"\n",
        "[[runtimes]] docker_vllm appears twice; one entry per runtime type"
    )]
    fn a_malformed_file_is_refused_naming_the_problem(
        #[case] text: &str,
        #[case] needle: &str,
    ) -> anyhow::Result<()> {
        let err = ExtraRuntimeConfigurations::parse(text)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should be refused: {text}"))?;
        let msg = format!("{err:#}");
        assert!(msg.contains(needle), "{msg}");
        Ok(())
    }

    /// What the engine gets is per runtime type, in file order; a type the
    /// client says nothing about gets nothing.
    #[test]
    fn envs_are_projected_per_runtime_type() -> anyhow::Result<()> {
        let config = ExtraRuntimeConfigurations::parse(FULL)?;
        let envs_of = |runtime: &Runtime| {
            config
                .for_runtime(runtime)
                .map_or(vec![], |c| spelled(c.envs()))
        };
        let (llamacpp, docker_vllm, mlx) = (llamacpp()?, docker_vllm()?, mlx()?);
        assert_eq!(
            envs_of(&llamacpp),
            ["CUDA_DEVICE_ORDER=PCI_BUS_ID", "CUDA_VISIBLE_DEVICES=1"]
        );
        assert_eq!(
            envs_of(&docker_vllm),
            ["VLLM_WORKER_MULTIPROC_METHOD=spawn"]
        );
        assert!(envs_of(&mlx).is_empty());
        Ok(())
    }

    fn entry_for(runtime_type: RuntimeType) -> anyhow::Result<ExtraRuntimeConfiguration> {
        Ok(ExtraRuntimeConfiguration::try_from(
            ExtraRuntimeConfigurationRef {
                runtime_type,
                envs: envs(&["A=1"])?,
            },
        )?)
    }

    #[rstest]
    #[case::llamacpp_apk(RuntimeType::LlamacppApkPipette)]
    #[case::llamacpp_ios(RuntimeType::LlamacppIosPipette)]
    #[case::mlx_ios(RuntimeType::MlxIosPipette)]
    #[case::onnx_genai_ios(RuntimeType::OnnxGenaiIosPipette)]
    #[case::apple_foundation(RuntimeType::AppleFoundation)]
    fn an_on_device_runtime_is_refused(#[case] runtime_type: RuntimeType) {
        assert_eq!(
            ExtraRuntimeConfiguration::try_from(ExtraRuntimeConfigurationRef {
                runtime_type,
                envs: vec![],
            }),
            Err(ExtraRuntimeConfigurationError::OnDeviceRuntime { runtime_type })
        );
    }

    /// Each runtime type answers one engine's accessor and no other.
    #[rstest]
    #[case::llamacpp(RuntimeType::LlamacppCliStockTools, "llama.cpp")]
    #[case::mlx(RuntimeType::MlxMacosPipette, "MLX")]
    #[case::docker_vllm(RuntimeType::DockerVllm, "torch-oai")]
    #[case::docker_sglang(RuntimeType::DockerSglang, "torch-oai")]
    #[case::uv_vllm(RuntimeType::UvVllm, "torch-oai")]
    #[case::uv_sglang(RuntimeType::UvSglang, "torch-oai")]
    #[case::openvino(RuntimeType::UvOpenvino, "OpenVINO")]
    #[case::onnx_genai(RuntimeType::UvOnnxGenai, "ONNX GenAI")]
    #[case::ovms(RuntimeType::Ovms, "OVMS")]
    #[case::trt_edgellm(RuntimeType::TrtEdgellm, "TensorRT Edge-LLM")]
    fn an_entry_is_only_readable_by_the_engine_it_is_for(
        #[case] runtime_type: RuntimeType,
        #[case] engine: &str,
    ) -> anyhow::Result<()> {
        let config = entry_for(runtime_type)?;
        assert_eq!(config.runtime_type(), runtime_type);
        let answers = [
            ("llama.cpp", config.as_llamacpp().map(|c| spelled(c.envs()))),
            ("MLX", config.as_mlx().map(|c| spelled(c.envs()))),
            (
                "torch-oai",
                config.as_torch_oai().map(|c| spelled(c.envs())),
            ),
            ("OpenVINO", config.as_openvino().map(|c| spelled(c.envs()))),
            (
                "ONNX GenAI",
                config.as_onnx_genai().map(|c| spelled(c.envs())),
            ),
            ("OVMS", config.as_ovms().map(|c| spelled(c.envs()))),
            (
                "TensorRT Edge-LLM",
                config.as_trt_edgellm().map(|c| spelled(c.envs())),
            ),
        ];
        for (reader, answer) in answers {
            let expected = if reader == engine {
                Ok(vec!["A=1".to_owned()])
            } else {
                Err(ExtraRuntimeConfigurationError::WrongEngine {
                    expected: reader,
                    got: runtime_type,
                })
            };
            assert_eq!(answer, expected, "{runtime_type} read by {reader}");
        }
        assert_eq!(spelled(config.envs()), ["A=1"], "{runtime_type}");
        Ok(())
    }

    /// The refusal names both sides, so a mis-wired dispatch is readable from
    /// the failure alone.
    #[test]
    fn a_foreign_entry_is_refused_naming_both_sides() -> anyhow::Result<()> {
        assert_eq!(
            entry_for(RuntimeType::DockerVllm)?
                .as_openvino()
                .err()
                .ok_or_else(|| anyhow::anyhow!("a vLLM entry is not OpenVINO's"))?
                .to_string(),
            "the extra runtime configuration for docker_vllm was handed to the OpenVINO engine"
        );
        Ok(())
    }

    /// A cell forwarding a variable the configuration sets is refused, in
    /// either form; an unrelated forward, or a runtime with no entry, is not.
    #[rstest]
    #[case::same_variable(
        &["CUDA_VISIBLE_DEVICES=0"],
        Some(
            "the cell's `CUDA_VISIBLE_DEVICES=0` env forward names a variable the client \
             configuration sets for llamacpp_cli_stock_tools; drop one of the two"
        )
    )]
    #[case::inherit_form(
        &["CUDA_VISIBLE_DEVICES"],
        Some(
            "the cell's `CUDA_VISIBLE_DEVICES` env forward names a variable the client \
             configuration sets for llamacpp_cli_stock_tools; drop one of the two"
        )
    )]
    #[case::unrelated(&["HF_TOKEN"], None)]
    #[case::none(&[], None)]
    fn a_cell_naming_a_configured_variable_is_refused(
        #[case] cell_envs: &[&str],
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let config = ExtraRuntimeConfigurations::parse(FULL)?;
        let result = config.admit(&spec(llamacpp()?, cell_envs)?);
        assert_eq!(
            result.err().map(|e| e.to_string()).as_deref(),
            refusal,
            "{cell_envs:?}"
        );
        config.admit(&spec(mlx()?, cell_envs)?)?;
        Ok(())
    }

    /// `runtime_type` goes, and a value that could be a credential never leaves
    /// the box.
    #[test]
    fn a_record_carries_names_and_the_ggml_values() -> anyhow::Result<()> {
        let config = ExtraRuntimeConfiguration::LlamacppCliStockTools(
            envs(&[
                "CUDA_VISIBLE_DEVICES=1",
                "GGML_CUDA_ENABLE_UNIFIED_MEMORY=0",
                "HF_TOKEN=hunter2",
            ])?
            .into(),
        );
        assert_eq!(
            config.submission_value(),
            serde_json::json!({
                "envs": [
                    "CUDA_VISIBLE_DEVICES",
                    "GGML_CUDA_ENABLE_UNIFIED_MEMORY=0",
                    "HF_TOKEN",
                ]
            })
        );
        Ok(())
    }

    /// Serializes as the authored entry, and a type this client never spawns
    /// cannot be read back from the wire.
    #[test]
    fn the_wire_form_is_the_authored_entry() -> anyhow::Result<()> {
        let config = ExtraRuntimeConfiguration::DockerVllm(
            envs(&["VLLM_WORKER_MULTIPROC_METHOD=spawn"])?.into(),
        );
        let value = serde_json::to_value(&config)?;
        assert_eq!(
            value,
            serde_json::json!({
                "runtime_type": "docker_vllm",
                "envs": ["VLLM_WORKER_MULTIPROC_METHOD=spawn"],
            })
        );
        assert_eq!(
            serde_json::from_value::<ExtraRuntimeConfiguration>(value)?,
            config
        );
        assert!(
            serde_json::from_value::<ExtraRuntimeConfiguration>(serde_json::json!({
                "runtime_type": "llamacpp_apk_pipette",
                "envs": [],
            }))
            .is_err()
        );
        Ok(())
    }

    /// The startup line names every variable and no value.
    #[test]
    fn display_names_variables_only() -> anyhow::Result<()> {
        let config = ExtraRuntimeConfigurations::parse(FULL)?;
        assert_eq!(
            config.to_string(),
            "docker_vllm: envs=[VLLM_WORKER_MULTIPROC_METHOD]; \
             llamacpp_cli_stock_tools: envs=[CUDA_DEVICE_ORDER, CUDA_VISIBLE_DEVICES]"
        );
        assert_eq!(ExtraRuntimeConfigurations::default().to_string(), "(none)");
        Ok(())
    }
}
