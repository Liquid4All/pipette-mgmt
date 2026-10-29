//! [`Runtime`], its per-kind variant structs, source coordinates and build
//! flavors: one submodule per engine; `uv` is the shared venv source.

mod llamacpp;
mod mlx;
mod onnx_genai;
mod openvino;
mod ovms;
mod torch_oai;
mod trt_edgellm;
mod uv;

pub use llamacpp::{
    LlamaCppFlavor, LlamacppApkPipette, LlamacppApkPipetteFlavor, LlamacppCliStockTools,
    LlamacppCliStockToolsRelease, LlamacppCliStockToolsSource, LlamacppFlashAttention,
    LlamacppIosPipette, LlamacppIosPipetteFlavor, LlamacppLoadMode,
};
pub use mlx::{
    MlxIosPipette, MlxIosPipetteFlavor, MlxMacosPipette, MlxMacosPipetteFlavor, MlxSwiftStack,
};
pub use onnx_genai::{
    OnnxExecutionProvider, OnnxGenaiIosPipette, OnnxGenaiIosPipetteFlavor, OnnxGenaiStack,
    UvOnnxGenai,
};
pub use openvino::{OpenvinoDevice, OpenvinoGenerateHint, UvOpenvino};
pub use ovms::{Ovms, OvmsFlavor, OvmsSource, OvmsSourceRepository};
pub use torch_oai::{
    DockerIpcMode, DockerSglang, DockerShmSize, DockerShmSizeError, DockerVllm, SglangFlavor,
    UvSglang, UvVllm, VllmDtype, VllmFlavor, VllmRepetitionDetection, flavor_from_uv_build,
};
pub use trt_edgellm::{TrtEdgellm, TrtEdgellmFlavor, TrtEdgellmPrecision, TrtEdgellmSource};
pub use uv::{
    SemverWithBuild, SemverWithBuildError, UvBuild, UvBuildError, UvPythonVersion,
    UvPythonVersionError, UvRuntimeSource, UvServerVersion, UvServerVersionError,
    uv_runtime_version,
};

use nutype::nutype;
use serde::{Deserialize, Serialize};

use crate::NonEmptyString;

/// Runtime declaration on a variant. vLLM and sglang are split per server so
/// each keeps its own flavor set.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Runtime {
    /// llama.cpp through the stock upstream CLI tools, on the desktop client.
    LlamacppCliStockTools(LlamacppCliStockTools),
    /// llama.cpp running in-process inside the Android pipette app.
    LlamacppApkPipette(LlamacppApkPipette),
    /// llama.cpp running in-process inside the iOS pipette app.
    LlamacppIosPipette(LlamacppIosPipette),
    MlxMacosPipette(MlxMacosPipette),
    /// MLX running in-process inside the iOS pipette app (mlx-swift).
    MlxIosPipette(MlxIosPipette),
    DockerVllm(DockerVllm),
    DockerSglang(DockerSglang),
    UvVllm(UvVllm),
    UvSglang(UvSglang),
    /// OpenVINO IR served by `openvino-genai` in a uv venv; the Intel NPU path.
    UvOpenvino(UvOpenvino),
    /// ONNX weights served by `onnxruntime-genai` in a uv venv, on one
    /// execution provider.
    UvOnnxGenai(UvOnnxGenai),
    /// OpenVINO IR served by an OpenVINO Model Server release archive and
    /// driven over HTTP. Unlike `UvOpenvino`, the server batches requests.
    Ovms(Ovms),
    /// ONNX Runtime GenAI running in-process inside the iOS pipette app.
    OnnxGenaiIosPipette(OnnxGenaiIosPipette),
    /// Apple Foundation Models (iOS); loads only `Model::AppleFoundationText`,
    /// and its `runtime_version` is the OS version.
    AppleFoundation(AppleFoundation),
    /// A TensorRT Edge-LLM server archive on a Jetson, driven over
    /// llama-server's HTTP dialect; its engines are built on the device.
    TrtEdgellm(TrtEdgellm),
}

/// The Apple Foundation runtime: the engine ships with the OS, so its only identity
/// is the app's private APIs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AppleFoundation {
    /// As [`LlamacppIosPipette::private_apis`]: they belong to the app, not the engine.
    #[serde(flatten)]
    pub private_apis: PrivateApis,
}

/// The private Apple APIs an internal iOS build compiled in; each changes what a number
/// means. `private_memory` is written only when it differs from `private_thermal`, so
/// existing descriptors and digests stay put.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(from = "PrivateApisWire", into = "PrivateApisWire")]
#[cfg_attr(feature = "schemars", schemars(with = "PrivateApisWire"))]
pub struct PrivateApis {
    /// The SoC die temperature through the private IOHID sensors. Without it the readiness
    /// gate has only `ProcessInfo.thermalState`, which stays `.nominal` into throttling.
    pub thermal: bool,
    /// libmalloc's reclaim-ring flush before each `max_memory_usage` bracket, and the
    /// kernel's footprint peak. Without the flush a cell reads earlier work's freed memory.
    pub memory: bool,
}

/// [`PrivateApis`] as plans and descriptors spell it.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
struct PrivateApisWire {
    /// See [`PrivateApis::thermal`]. Absent means `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    private_thermal: bool,
    /// See [`PrivateApis::memory`]. Absent means the same as `private_thermal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_memory: Option<bool>,
}

impl From<PrivateApisWire> for PrivateApis {
    fn from(wire: PrivateApisWire) -> Self {
        Self {
            thermal: wire.private_thermal,
            memory: wire.private_memory.unwrap_or(wire.private_thermal),
        }
    }
}

impl From<PrivateApis> for PrivateApisWire {
    fn from(apis: PrivateApis) -> Self {
        Self {
            private_thermal: apis.thermal,
            private_memory: (apis.memory != apis.thermal).then_some(apis.memory),
        }
    }
}

/// Normalize any pasted repository coordinate (HTTPS, SSH, scp-like, `.git`) to
/// `<host>/<org>/<repo>`, so every form stores and compares identically.
fn strip_url_scheme(raw: String) -> String {
    let mut s = raw.trim();

    if let Some(rest) = ["https://", "http://", "ssh://", "git://"]
        .into_iter()
        .find_map(|scheme| s.strip_prefix(scheme))
    {
        s = rest;
    }

    // Userinfo: a `@` before the first path `/`.
    if let Some((userinfo, rest)) = s.split_once('@')
        && !userinfo.contains('/')
    {
        s = rest;
    }

    // scp-like `host:org/repo`: the first `:` when no `/` precedes it.
    let colon_normalized = match s.split_once(':') {
        Some((host, path)) if !host.contains('/') => format!("{host}/{path}"),
        _ => s.to_owned(),
    };

    let trimmed = colon_normalized.trim_end_matches('/');
    trimmed
        .strip_suffix(".git")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_owned()
}

/// A source repository as `<host>/<org>/<repo>` (`github.com/ggml-org/llama.cpp`);
/// any pasted URL form is normalized to it.
#[nutype(
    sanitize(with = strip_url_scheme),
    derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, AsRef, Display)
)]
pub struct RepositoryUrl(String);

impl RepositoryUrl {
    /// The `<org>/<repo>` portion, dropping the leading `<host>/` segment.
    pub fn org_repo(&self) -> &str {
        let s: &str = self.as_ref();
        s.split_once('/').map(|(_, rest)| rest).unwrap_or(s)
    }
}

/// A source checkout: the repo URL plus the ref (a tag like `b5000` or a commit)
/// built from it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SourceRepository {
    #[serde(alias = "source_repo")]
    pub repository_url: RepositoryUrl,
    #[serde(alias = "version")]
    pub repository_version: NonEmptyString,
}

impl Runtime {
    /// The iOS `headlessrun runtime=<token>` value: the plan's `type` tag.
    pub fn headless_token(&self) -> &'static str {
        match self {
            Runtime::LlamacppCliStockTools(_) => "llamacpp_cli_stock_tools",
            Runtime::LlamacppApkPipette(_) => "llamacpp_apk_pipette",
            Runtime::LlamacppIosPipette(_) => "llamacpp_ios_pipette",
            Runtime::MlxMacosPipette(_) => "mlx_macos_pipette",
            Runtime::MlxIosPipette(_) => "mlx_ios_pipette",
            Runtime::DockerVllm(_) => "docker_vllm",
            Runtime::DockerSglang(_) => "docker_sglang",
            Runtime::UvVllm(_) => "uv_vllm",
            Runtime::UvSglang(_) => "uv_sglang",
            Runtime::UvOpenvino(_) => "uv_openvino",
            Runtime::UvOnnxGenai(_) => "uv_onnx_genai",
            Runtime::Ovms(_) => "ovms",
            Runtime::OnnxGenaiIosPipette(_) => "onnx_genai_ios_pipette",
            Runtime::AppleFoundation(_) => "apple_foundation",
            Runtime::TrtEdgellm(_) => "trt_edgellm",
        }
    }

    /// The `--runtime <ref>` the runtime binary takes. Narrower than
    /// [`Display`](std::fmt::Display): the binary resolves by version and flavor,
    /// so the source repo would break that lookup.
    pub fn cli_ref(&self) -> String {
        match self {
            Runtime::LlamacppCliStockTools(rt) => {
                format!("{}:{}", rt.source.reference(), rt.flavor)
            }
            Runtime::LlamacppApkPipette(rt) => {
                format!("{}:{}", rt.source.repository_version, rt.flavor)
            }
            Runtime::LlamacppIosPipette(rt) => {
                format!("{}:{}", rt.source.repository_version, rt.flavor)
            }
            // The remaining variants' `Display` already is the binary ref form.
            other @ (Runtime::MlxMacosPipette(_)
            | Runtime::MlxIosPipette(_)
            | Runtime::DockerVllm(_)
            | Runtime::DockerSglang(_)
            | Runtime::UvVllm(_)
            | Runtime::UvSglang(_)
            | Runtime::UvOpenvino(_)
            | Runtime::UvOnnxGenai(_)
            | Runtime::Ovms(_)
            | Runtime::OnnxGenaiIosPipette(_)
            | Runtime::AppleFoundation(_)
            | Runtime::TrtEdgellm(_)) => other.to_string(),
        }
    }
}

/// Reduce a `--runtime <arg>`, either `Runtime` JSON or a flat ref, to its
/// [`Runtime::cli_ref`].
pub fn cli_ref_from_runtime_arg(arg: &str) -> Result<String, serde_json::Error> {
    if arg.trim_start().starts_with('{') {
        let runtime: Runtime = serde_json::from_str(arg)?;
        Ok(runtime.cli_ref())
    } else {
        Ok(arg.to_string())
    }
}

/// `<repository_url>@<repository_version>`: the full coordinate, so forks differ.
impl std::fmt::Display for SourceRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.repository_url, self.repository_version)
    }
}

impl Runtime {
    /// The venv source of a uv-hosted runtime.
    pub fn uv_source(&self) -> Option<&UvRuntimeSource> {
        match self {
            Runtime::UvVllm(rt) => Some(&rt.source),
            Runtime::UvSglang(rt) => Some(&rt.source),
            Runtime::UvOpenvino(rt) => Some(&rt.source),
            Runtime::UvOnnxGenai(rt) => Some(&rt.source),
            Runtime::MlxMacosPipette(rt) => Some(&rt.source),
            Runtime::LlamacppCliStockTools(_)
            | Runtime::LlamacppApkPipette(_)
            | Runtime::LlamacppIosPipette(_)
            | Runtime::MlxIosPipette(_)
            | Runtime::OnnxGenaiIosPipette(_)
            | Runtime::DockerVllm(_)
            | Runtime::DockerSglang(_)
            | Runtime::Ovms(_)
            | Runtime::AppleFoundation(_)
            | Runtime::TrtEdgellm(_) => None,
        }
    }
}

/// The canonical string for a runtime: the state `runtime_ref`, the cell-hash
/// key, and what logs show. It carries the full source origin, not just its ref.
impl std::fmt::Display for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Runtime::LlamacppCliStockTools(rt) => rt.fmt(f),
            Runtime::LlamacppApkPipette(rt) => rt.fmt(f),
            Runtime::LlamacppIosPipette(rt) => rt.fmt(f),
            Runtime::MlxMacosPipette(rt) => rt.fmt(f),
            Runtime::MlxIosPipette(rt) => rt.fmt(f),
            Runtime::DockerVllm(rt) => rt.fmt(f),
            Runtime::DockerSglang(rt) => rt.fmt(f),
            Runtime::UvVllm(rt) => rt.fmt(f),
            Runtime::UvSglang(rt) => rt.fmt(f),
            Runtime::UvOpenvino(rt) => rt.fmt(f),
            Runtime::UvOnnxGenai(rt) => rt.fmt(f),
            Runtime::Ovms(rt) => rt.fmt(f),
            Runtime::OnnxGenaiIosPipette(rt) => rt.fmt(f),
            Runtime::AppleFoundation(_) => write!(f, "apple_foundation"),
            Runtime::TrtEdgellm(rt) => rt.fmt(f),
        }
    }
}

/// Runtime-type discriminant, 1:1 with the [`Runtime`] enum's variants.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    strum::Display,
    strum::EnumString,
    strum::EnumIter,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RuntimeType {
    LlamacppCliStockTools,
    LlamacppApkPipette,
    LlamacppIosPipette,
    MlxMacosPipette,
    MlxIosPipette,
    DockerVllm,
    DockerSglang,
    UvVllm,
    UvSglang,
    UvOpenvino,
    UvOnnxGenai,
    Ovms,
    OnnxGenaiIosPipette,
    AppleFoundation,
    TrtEdgellm,
}

impl RuntimeType {
    /// The type of a concrete [`Runtime`].
    pub fn of(runtime: &Runtime) -> Self {
        match runtime {
            Runtime::LlamacppCliStockTools(_) => Self::LlamacppCliStockTools,
            Runtime::LlamacppApkPipette(_) => Self::LlamacppApkPipette,
            Runtime::LlamacppIosPipette(_) => Self::LlamacppIosPipette,
            Runtime::MlxMacosPipette(_) => Self::MlxMacosPipette,
            Runtime::MlxIosPipette(_) => Self::MlxIosPipette,
            Runtime::DockerVllm(_) => Self::DockerVllm,
            Runtime::DockerSglang(_) => Self::DockerSglang,
            Runtime::UvVllm(_) => Self::UvVllm,
            Runtime::UvSglang(_) => Self::UvSglang,
            Runtime::UvOpenvino(_) => Self::UvOpenvino,
            Runtime::UvOnnxGenai(_) => Self::UvOnnxGenai,
            Runtime::Ovms(_) => Self::Ovms,
            Runtime::OnnxGenaiIosPipette(_) => Self::OnnxGenaiIosPipette,
            Runtime::AppleFoundation(_) => Self::AppleFoundation,
            Runtime::TrtEdgellm(_) => Self::TrtEdgellm,
        }
    }
}

/// A POSIX environment-variable name: the portable set, since an entry reaches
/// `Command::env` on three platforms and `docker -e`.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One `envs` entry: `NAME=VALUE` (split on the first `=`; no NUL), or a bare
/// `NAME` to inherit. The entry is trimmed, so a trailing space in a value is lost.
#[nutype(
    sanitize(trim),
    validate(predicate = is_env_forward),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct EnvForward(String);

fn is_env_forward(s: &str) -> bool {
    match s.split_once('=') {
        Some((name, value)) => is_env_name(name) && !value.contains('\0'),
        None => is_env_name(s),
    }
}

impl EnvForward {
    /// The variable name.
    pub fn name(&self) -> &str {
        let s: &str = self.as_ref();
        s.split_once('=').map_or(s, |(name, _)| name)
    }

    /// The value, or `None` to inherit. `Some("")` is set-but-empty, which some
    /// `GGML_*` knobs branch on; the docker/uv launchers treat it as inherit.
    pub fn value(&self) -> Option<&str> {
        let s: &str = self.as_ref();
        s.split_once('=').map(|(_, value)| value)
    }

    /// This forward with its value dropped, leaving the bare name.
    pub fn redacted(&self) -> Self {
        Self::try_new(self.name().to_owned()).unwrap_or_else(|_| self.clone())
    }

    /// This forward as a result record carries it: [`Self::redacted`], except
    /// in the `GGML_*` namespace. The one place that rule is spelled.
    pub fn for_record(&self) -> Self {
        if self.name().starts_with("GGML_") {
            self.clone()
        } else {
            self.redacted()
        }
    }
}

/// A `SourceRepository` fixture at `url` and the given ref.
#[cfg(test)]
pub(crate) fn source_repo(url: &str, version: &str) -> anyhow::Result<SourceRepository> {
    Ok(SourceRepository {
        repository_url: RepositoryUrl::new(url),
        repository_version: NonEmptyString::try_new(version.to_owned())?,
    })
}

/// A [`source_repo`] at upstream llama.cpp, the llama.cpp variants' fixture.
#[cfg(test)]
pub(crate) fn app_source(version: &str) -> anyhow::Result<SourceRepository> {
    source_repo("github.com/ggml-org/llama.cpp", version)
}

/// An upstream llama.cpp release that names no asset, the CLI variant's fixture.
#[cfg(test)]
pub(crate) fn cli_release(version: &str) -> anyhow::Result<LlamacppCliStockToolsRelease> {
    let SourceRepository {
        repository_url,
        repository_version,
    } = app_source(version)?;
    Ok(LlamacppCliStockToolsRelease {
        repository_url,
        repository_version,
        asset: None,
    })
}

/// Every variant's `as_str` is its serde spelling, and that word parses back.
#[cfg(test)]
pub(crate) fn words_match_serde<E>(as_str: fn(&E) -> &'static str) -> anyhow::Result<()>
where
    E: strum::IntoEnumIterator
        + Serialize
        + serde::de::DeserializeOwned
        + PartialEq
        + std::fmt::Debug,
{
    E::iter().try_for_each(|e| {
        let word = as_str(&e);
        anyhow::ensure!(
            serde_json::to_value(&e)? == word,
            "{e:?} serializes apart from {word:?}"
        );
        anyhow::ensure!(
            serde_json::from_value::<E>(word.into())? == e,
            "{word:?} does not parse back to {e:?}"
        );
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use super::uv::sample_catalog_source;
    use super::*;
    use crate::*;

    #[rstest]
    #[case::stock(json!({}), false, false)]
    #[case::internal(json!({"private_thermal": true}), true, true)]
    #[case::thermal_only(json!({"private_thermal": true, "private_memory": false}), true, false)]
    #[case::memory_only(json!({"private_memory": true}), false, true)]
    fn private_memory_defaults_to_private_thermal(
        #[case] apis: serde_json::Value,
        #[case] thermal: bool,
        #[case] memory: bool,
    ) -> anyhow::Result<()> {
        let mut wire = apis;
        wire["type"] = "apple_foundation".into();
        let expected = Runtime::AppleFoundation(AppleFoundation {
            private_apis: PrivateApis { thermal, memory },
        });

        assert_eq!(serde_json::from_value::<Runtime>(wire.clone())?, expected);
        assert_eq!(serde_json::to_value(&expected)?, wire);
        Ok(())
    }

    /// A descriptor written either way has one digest.
    #[rstest]
    #[case::internal(json!({"type": "apple_foundation", "private_thermal": true, "private_memory": true}))]
    #[case::stock(json!({"type": "apple_foundation", "private_memory": false}))]
    fn a_redundant_private_memory_is_dropped(
        #[case] wire: serde_json::Value,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = serde_json::from_value(wire)?;
        let written = serde_json::to_value(&runtime)?;
        assert!(written.get("private_memory").is_none(), "{written}");
        Ok(())
    }

    #[rstest]
    #[case("github.com/ggml-org/llama.cpp")]
    #[case("https://github.com/ggml-org/llama.cpp")]
    #[case("http://github.com/ggml-org/llama.cpp")]
    #[case("https://github.com/ggml-org/llama.cpp/")]
    #[case("https://github.com/ggml-org/llama.cpp.git")]
    #[case("  https://github.com/ggml-org/llama.cpp.git/  ")]
    #[case("git@github.com:ggml-org/llama.cpp.git")]
    #[case("git@github.com:ggml-org/llama.cpp")]
    #[case("ssh://git@github.com/ggml-org/llama.cpp.git")]
    fn repository_url_normalizes_every_paste_form(#[case] raw: &str) {
        assert_eq!(
            RepositoryUrl::new(raw).as_ref(),
            "github.com/ggml-org/llama.cpp",
            "`{raw}` should normalize to the canonical form"
        );
    }

    #[test]
    fn repository_url_is_host_agnostic() {
        let repo = RepositoryUrl::new("git@gitlab.com:acme/fork.git");
        assert_eq!(repo.as_ref(), "gitlab.com/acme/fork");
        assert_eq!(repo.org_repo(), "acme/fork");
    }

    /// No repo has a default, however the `SourceRepository` is reached.
    #[rstest]
    #[case::cli_github_release(
        r#"{"type":"llamacpp_cli_stock_tools","source":"github_release","repository_version":"b1","flavor":"macos-arm64"}"#
    )]
    #[case::apk_flattened(
        r#"{"type":"llamacpp_apk_pipette","repository_version":"b1","flavor":"android-arm64-v8"}"#
    )]
    #[case::ios_flattened(
        r#"{"type":"llamacpp_ios_pipette","repository_version":"b1","flavor":"ios-arm64"}"#
    )]
    #[case::mlx_ios_package(
        r#"{"type":"mlx_ios_pipette","flavor":"ios-arm64","packages":{"mlx_swift":{"repository_version":"1"},"mlx_swift_lm":{"repository_url":"github.com/ml-explore/mlx-swift-examples","repository_version":"1"},"swift_transformers":{"repository_url":"github.com/huggingface/swift-transformers","repository_version":"1"}}}"#
    )]
    #[case::onnx_genai_ios_package(
        r#"{"type":"onnx_genai_ios_pipette","flavor":"ios-arm64","packages":{"onnxruntime":{"repository_version":"v1.23.0"},"onnxruntime_genai":{"repository_url":"github.com/microsoft/onnxruntime-genai","repository_version":"v0.15.2"}}}"#
    )]
    fn a_source_repository_without_a_url_is_refused(#[case] json: &str) -> anyhow::Result<()> {
        let Err(err) = serde_json::from_str::<Runtime>(json) else {
            anyhow::bail!("should not decode: {json}");
        };
        assert!(
            err.to_string().contains("missing field `repository_url`"),
            "{json}: {err}"
        );
        Ok(())
    }

    #[test]
    fn cli_ref_from_runtime_arg_accepts_either_form() -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(
            "type = \"llamacpp_cli_stock_tools\"\nsource = \"github_release\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b9305\"\nflavor = \"macos-arm64\"",
        )?;
        let json = serde_json::to_string(&runtime)?;
        assert_eq!(
            cli_ref_from_runtime_arg(&format!(" {json}"))?,
            runtime.cli_ref()
        );
        assert_eq!(
            cli_ref_from_runtime_arg(&runtime.cli_ref())?,
            runtime.cli_ref()
        );
        assert!(cli_ref_from_runtime_arg("{not json").is_err());
        Ok(())
    }

    /// Only the scp-like host separator is rewritten; a `:` in the path stays.
    #[test]
    fn repository_url_keeps_a_colon_after_the_host() {
        let repo = RepositoryUrl::new("https://example.test/acme/fork:v2");
        assert_eq!(repo.as_ref(), "example.test/acme/fork:v2");
    }

    /// A change to any kind's `Display` moves stored keys.
    #[rstest]
    #[case::llamacpp_cli(
        "type = \"llamacpp_cli_stock_tools\"\nsource = \"github_release\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b9305\"\nflavor = \"macos-arm64\"",
        "github.com/ggml-org/llama.cpp@b9305:macos-arm64",
        false
    )]
    #[case::llamacpp_apk(
        "type = \"llamacpp_apk_pipette\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b9050\"\nflavor = \"android-arm64-v8\"",
        "github.com/ggml-org/llama.cpp@b9050:android-arm64-v8",
        false
    )]
    #[case::llamacpp_ios(
        "type = \"llamacpp_ios_pipette\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b9050\"\nflavor = \"ios-arm64\"",
        "github.com/ggml-org/llama.cpp@b9050:ios-arm64",
        false
    )]
    #[case::mlx_macos(
        "type = \"mlx_macos_pipette\"\nversion = \"0.20.0\"\nflavor = \"macos-arm64\"\nsource = { type = \"pip_requirements_text\", contents = \"mlx-lm==0.20.0\" }",
        "0.20.0",
        true
    )]
    #[case::mlx_ios(
        "type = \"mlx_ios_pipette\"\nflavor = \"ios-arm64\"\n[packages]\nmlx_swift = { repository_url = \"github.com/ml-explore/mlx-swift\", repository_version = \"0.25.6\" }\nmlx_swift_lm = { repository_url = \"github.com/ml-explore/mlx-swift-examples\", repository_version = \"2.25.6\" }\nswift_transformers = { repository_url = \"github.com/huggingface/swift-transformers\", repository_version = \"0.1.22\" }",
        "mlx-swift=github.com/ml-explore/mlx-swift@0.25.6 mlx-swift-lm=github.com/ml-explore/mlx-swift-examples@2.25.6 swift-transformers=github.com/huggingface/swift-transformers@0.1.22:ios-arm64",
        false
    )]
    #[case::onnx_genai_ios(
        "type = \"onnx_genai_ios_pipette\"\nflavor = \"ios-arm64\"\n[packages]\nonnxruntime = { repository_url = \"github.com/microsoft/onnxruntime\", repository_version = \"v1.23.0\" }\nonnxruntime_genai = { repository_url = \"github.com/microsoft/onnxruntime-genai\", repository_version = \"v0.15.2\" }",
        "onnxruntime=github.com/microsoft/onnxruntime@v1.23.0 onnxruntime-genai=github.com/microsoft/onnxruntime-genai@v0.15.2:ios-arm64",
        false
    )]
    #[case::docker_vllm(
        "type = \"docker_vllm\"\nimage_name = \"vllm/vllm-openai\"\nimage_tag = \"v0.20.2\"\nflavor = \"nvidia_gpu\"",
        "docker://vllm/vllm-openai:v0.20.2",
        false
    )]
    #[case::docker_sglang(
        "type = \"docker_sglang\"\nimage_name = \"lmsysorg/sglang\"\nimage_tag = \"v0.5.15-cu130\"\nflavor = \"nvidia_gpu\"",
        "docker://lmsysorg/sglang:v0.5.15-cu130",
        false
    )]
    #[case::uv_vllm(
        "type = \"uv_vllm\"\nserver_version = \"0.10.0\"\nbuild = \"cu121\"\npython_version = \"3.12\"\nsource = { type = \"pip_requirements_text\", contents = \"vllm==0.10.0\" }",
        "uv://vllm@0.10.0+cu121.py3.12",
        true
    )]
    #[case::uv_sglang(
        "type = \"uv_sglang\"\nserver_version = \"0.5.12\"\nbuild = \"cu121\"\npython_version = \"3.12\"\nsource = { type = \"pip_requirements_text\", contents = \"sglang==0.5.12\" }",
        "uv://sglang@0.5.12+cu121.py3.12",
        true
    )]
    #[case::uv_openvino(
        "type = \"uv_openvino\"\nserver_version = \"2026.2.1\"\npython_version = \"3.11\"\nsource = { type = \"pip_requirements_text\", contents = \"openvino-genai==2026.2.1.0\" }",
        "uv://openvino@2026.2.1.py3.11",
        true
    )]
    #[case::uv_onnx_genai(
        "type = \"uv_onnx_genai\"\nserver_version = \"0.16.0\"\nexecution_provider = \"cuda\"\npython_version = \"3.11\"\nsource = { type = \"pip_requirements_text\", contents = \"onnxruntime-genai-cuda==0.16.0\" }",
        "uv://onnx-genai@0.16.0+cuda.py3.11",
        true
    )]
    #[case::ovms(
        "type = \"ovms\"\nsource = \"github_release\"\nrepository_url = \"github.com/openvinotoolkit/model_server\"\nversion = \"v2026.4.0\"\nflavor = \"windows-x64\"",
        "github.com/openvinotoolkit/model_server@v2026.4.0:windows-x64",
        false
    )]
    #[case::apple_foundation("type = \"apple_foundation\"", "apple_foundation", false)]
    #[case::trt_edgellm(
        "type = \"trt_edgellm\"\nsource = \"relative_dir\"\ndir = \"blobs\"\nflavor = \"jetson-orin-cuda-13\"",
        "relative-dir:blobs:jetson-orin-cuda-13",
        false
    )]
    fn every_runtime_kind_renders_its_canonical_id(
        #[case] runtime_toml: &str,
        #[case] expected: &str,
        #[case] installs_into_a_venv: bool,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(runtime_toml)?;
        assert_eq!(runtime.to_string(), expected);
        assert_eq!(runtime.uv_source().is_some(), installs_into_a_venv);
        Ok(())
    }

    /// One runtime of each kind, in `RuntimeType` declaration order.
    fn one_of_each_kind() -> anyhow::Result<Vec<Runtime>> {
        Ok(vec![
            Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
                source: LlamacppCliStockToolsSource::GithubRelease(cli_release("b9050")?),
                flavor: LlamaCppFlavor::MacosArm64,
            }),
            Runtime::LlamacppApkPipette(LlamacppApkPipette {
                source: app_source("b1")?,
                flavor: LlamacppApkPipetteFlavor::AndroidArm64V8,
            }),
            Runtime::LlamacppIosPipette(LlamacppIosPipette {
                source: app_source("b1")?,
                flavor: LlamacppIosPipetteFlavor::IosArm64,
                private_apis: PrivateApis::default(),
            }),
            Runtime::MlxMacosPipette(MlxMacosPipette {
                version: NonEmptyString::try_new("0.31".to_owned())?,
                flavor: MlxMacosPipetteFlavor::MacosArm64,
                source: sample_catalog_source()?,
            }),
            Runtime::MlxIosPipette(MlxIosPipette {
                packages: MlxSwiftStack {
                    mlx_swift: source_repo("github.com/ml-explore/mlx-swift", "0.25.6")?,
                    mlx_swift_lm: source_repo(
                        "github.com/ml-explore/mlx-swift-examples",
                        "2.25.6",
                    )?,
                    swift_transformers: source_repo(
                        "github.com/huggingface/swift-transformers",
                        "0.1.22",
                    )?,
                },
                flavor: MlxIosPipetteFlavor::IosArm64,
                private_apis: PrivateApis {
                    thermal: true,
                    memory: true,
                },
            }),
            Runtime::DockerVllm(DockerVllm {
                image_name: NonEmptyString::try_new("vllm/vllm-openai")?,
                image_tag: NonEmptyString::try_new("v0.20.2")?,
                flavor: VllmFlavor::NvidiaGpu,
            }),
            Runtime::DockerSglang(DockerSglang {
                image_name: NonEmptyString::try_new("lmsysorg/sglang")?,
                image_tag: NonEmptyString::try_new("v0.5.15-cu130")?,
                flavor: SglangFlavor::NvidiaGpu,
            }),
            Runtime::UvVllm(UvVllm {
                server_version: UvServerVersion::try_new("0.10.0".to_owned())?,
                build: UvBuild::try_new("cu121".to_owned())?,
                python_version: UvPythonVersion::try_new("3.12".to_owned())?,
                source: sample_catalog_source()?,
            }),
            Runtime::UvSglang(UvSglang {
                server_version: UvServerVersion::try_new("0.5.12")?,
                build: UvBuild::try_new("cu121")?,
                python_version: UvPythonVersion::try_new("3.12")?,
                source: sample_catalog_source()?,
            }),
            Runtime::UvOpenvino(UvOpenvino {
                server_version: UvServerVersion::try_new("2026.2.1")?,
                python_version: UvPythonVersion::try_new("3.11")?,
                source: sample_catalog_source()?,
            }),
            Runtime::UvOnnxGenai(UvOnnxGenai {
                server_version: UvServerVersion::try_new("0.16.0".to_owned())?,
                execution_provider: OnnxExecutionProvider::Cpu,
                python_version: UvPythonVersion::try_new("3.11".to_owned())?,
                source: sample_catalog_source()?,
            }),
            Runtime::Ovms(Ovms {
                source: OvmsSource::GithubRelease(OvmsSourceRepository {
                    repository_url: RepositoryUrl::new("github.com/openvinotoolkit/model_server"),
                    repository_version: NonEmptyString::try_new("v2026.4.0".to_owned())?,
                }),
                flavor: OvmsFlavor::WindowsX64,
            }),
            Runtime::OnnxGenaiIosPipette(OnnxGenaiIosPipette {
                packages: OnnxGenaiStack {
                    onnxruntime: source_repo("github.com/microsoft/onnxruntime", "v1.23.0")?,
                    onnxruntime_genai: source_repo(
                        "github.com/microsoft/onnxruntime-genai",
                        "v0.15.2",
                    )?,
                },
                flavor: OnnxGenaiIosPipetteFlavor::IosArm64,
                private_apis: PrivateApis {
                    thermal: true,
                    memory: true,
                },
            }),
            Runtime::AppleFoundation(Default::default()),
            Runtime::TrtEdgellm(TrtEdgellm {
                source: TrtEdgellmSource::RelativeDir {
                    dir: RelativePath::try_new("blobs")?,
                },
                flavor: TrtEdgellmFlavor::JetsonOrinCuda13,
            }),
        ])
    }

    fn type_tag(runtime: &Runtime) -> anyhow::Result<String> {
        serde_json::to_value(runtime)?
            .get("type")
            .and_then(|t| t.as_str())
            .map(str::to_owned)
            .context("runtime serde value has a string `type` tag")
    }

    #[test]
    fn headless_token_matches_plan_type_tag() -> anyhow::Result<()> {
        for rt in one_of_each_kind()? {
            assert_eq!(rt.headless_token(), type_tag(&rt)?, "drift for {rt:?}");
        }
        Ok(())
    }

    #[test]
    fn runtime_type_is_spelled_as_the_type_tag() -> anyhow::Result<()> {
        use strum::IntoEnumIterator as _;

        let runtimes = one_of_each_kind()?;
        for rt in &runtimes {
            let tag = type_tag(rt)?;
            let kind = RuntimeType::of(rt);
            assert_eq!(serde_json::to_value(kind)?, tag.as_str(), "{rt:?}");
            assert_eq!(
                serde_json::from_value::<RuntimeType>(tag.as_str().into())?,
                kind
            );
            assert_eq!(kind.to_string(), tag);
        }
        // Distinct kinds in declaration order, so the fixture misses none.
        assert_eq!(
            runtimes.iter().map(RuntimeType::of).collect::<Vec<_>>(),
            RuntimeType::iter().collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn cli_ref_is_what_the_binary_resolves() -> anyhow::Result<()> {
        for rt in one_of_each_kind()? {
            let expected = match RuntimeType::of(&rt) {
                RuntimeType::LlamacppCliStockTools => "b9050:macos-arm64".to_owned(),
                RuntimeType::LlamacppApkPipette => "b1:android-arm64-v8".to_owned(),
                RuntimeType::LlamacppIosPipette => "b1:ios-arm64".to_owned(),
                RuntimeType::MlxMacosPipette
                | RuntimeType::MlxIosPipette
                | RuntimeType::DockerVllm
                | RuntimeType::DockerSglang
                | RuntimeType::UvVllm
                | RuntimeType::UvSglang
                | RuntimeType::UvOpenvino
                | RuntimeType::UvOnnxGenai
                | RuntimeType::Ovms
                | RuntimeType::OnnxGenaiIosPipette
                | RuntimeType::AppleFoundation
                | RuntimeType::TrtEdgellm => rt.to_string(),
            };
            assert_eq!(rt.cli_ref(), expected, "{rt:?}");
        }
        Ok(())
    }

    #[rstest::rstest]
    #[case("GGML_HEXAGON_OPPOLL=1", true)]
    #[case("CUDA_VISIBLE_DEVICES", true)]
    #[case("_LEADING_UNDERSCORE=x", true)]
    #[case("OPTS=a=b", true)]
    #[case("GGML_HEXAGON_NHVX=", true)]
    // Trimmed, not rejected.
    #[case("  GGML_HEXAGON_OPPOLL=1  ", true)]
    // Refused at plan time rather than failing at spawn.
    #[case("", false)]
    #[case("=novalue", false)]
    #[case("9LEADING_DIGIT=x", false)]
    #[case("HAS SPACE=x", false)]
    #[case("HAS-DASH=x", false)]
    #[case("lower_is_fine_too=x", true)]
    #[case("HAS.DOT=x", false)]
    #[case("NUL_IN_VALUE=a\0b", false)]
    fn env_forward_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(EnvForward::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    /// A record keeps a value only under the exact, case-sensitive `GGML_` prefix.
    #[rstest::rstest]
    #[case(
        "GGML_HEXAGON_OPPOLL=1",
        "GGML_HEXAGON_OPPOLL",
        Some("1"),
        "GGML_HEXAGON_OPPOLL=1"
    )]
    #[case("OPTS=a=b", "OPTS", Some("a=b"), "OPTS")]
    #[case(
        "GGML_HEXAGON_NHVX=",
        "GGML_HEXAGON_NHVX",
        Some(""),
        "GGML_HEXAGON_NHVX="
    )]
    #[case("HF_TOKEN=hunter2", "HF_TOKEN", Some("hunter2"), "HF_TOKEN")]
    #[case("GGMLX=1", "GGMLX", Some("1"), "GGMLX")]
    #[case("ggml_lower=1", "ggml_lower", Some("1"), "ggml_lower")]
    #[case(
        "CUDA_VISIBLE_DEVICES",
        "CUDA_VISIBLE_DEVICES",
        None,
        "CUDA_VISIBLE_DEVICES"
    )]
    #[case("GGML_INHERITED", "GGML_INHERITED", None, "GGML_INHERITED")]
    fn env_forward_splits_name_from_value(
        #[case] s: &str,
        #[case] name: &str,
        #[case] value: Option<&str>,
        #[case] record: &str,
    ) -> anyhow::Result<()> {
        let forward = EnvForward::try_new(s.to_owned())?;
        assert_eq!(forward.name(), name);
        assert_eq!(forward.value(), value);
        assert_eq!(forward.redacted().as_ref(), name);
        assert_eq!(forward.redacted().value(), None);
        assert_eq!(forward.for_record().as_ref(), record);
        Ok(())
    }
}
