//! vLLM and SGLang, under docker or uv: the runtimes the torch-oai engine drives.

use nutype::nutype;
use serde::{Deserialize, Serialize};

use crate::{
    NonEmptyString, PositiveCount, UvBuild, UvPythonVersion, UvRuntimeSource, UvServerVersion,
    uv_runtime_version,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DockerVllm {
    pub image_name: NonEmptyString,
    pub image_tag: NonEmptyString,
    pub flavor: VllmFlavor,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DockerSglang {
    pub image_name: NonEmptyString,
    pub image_tag: NonEmptyString,
    pub flavor: SglangFlavor,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UvVllm {
    pub server_version: UvServerVersion,
    pub build: UvBuild,
    pub python_version: UvPythonVersion,
    pub source: UvRuntimeSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UvSglang {
    pub server_version: UvServerVersion,
    pub build: UvBuild,
    pub python_version: UvPythonVersion,
    pub source: UvRuntimeSource,
}

impl UvVllm {
    pub fn runtime_version(&self) -> String {
        uv_runtime_version(&self.server_version, &self.build, &self.python_version)
    }
}

impl UvSglang {
    pub fn runtime_version(&self) -> String {
        uv_runtime_version(&self.server_version, &self.build, &self.python_version)
    }
}

/// The GPU target a uv build tag (`cu121`, `rocm624`, `cpu`) names, as
/// [`VllmFlavor`] for SGLang builds too. Here so the installer and the client,
/// which both classify a tag, cannot spell it two ways.
pub fn flavor_from_uv_build(build: &UvBuild) -> VllmFlavor {
    let build = build.as_ref();
    if build == "cpu" {
        VllmFlavor::Cpu
    } else if build.starts_with("rocm") {
        VllmFlavor::AmdGpu
    } else {
        VllmFlavor::NvidiaGpu
    }
}

impl std::fmt::Display for DockerVllm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "docker://{}:{}", self.image_name, self.image_tag)
    }
}

impl std::fmt::Display for DockerSglang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "docker://{}:{}", self.image_name, self.image_tag)
    }
}

/// `uv://vllm@<version>` — wire form for torch-oai's `--runtime` arg.
impl std::fmt::Display for UvVllm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uv://vllm@{}", self.runtime_version())
    }
}

/// `uv://sglang@<version>` — wire form for torch-oai's `--runtime` arg.
impl std::fmt::Display for UvSglang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uv://sglang@{}", self.runtime_version())
    }
}

/// Closed set of vllm build targets. Separate from `SglangFlavor`, though
/// identical today, so a server-specific target can land on one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "clap", clap(rename_all = "snake_case"))]
pub enum VllmFlavor {
    NvidiaGpu,
    AmdGpu,
    Cpu,
}

/// Closed set of sglang build targets.  See `VllmFlavor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "clap", clap(rename_all = "snake_case"))]
pub enum SglangFlavor {
    NvidiaGpu,
    AmdGpu,
    Cpu,
}

/// vLLM's `--dtype`, closed so a typo fails at plan parse rather than at launch.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    strum::EnumIter,
    strum::IntoStaticStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum VllmDtype {
    Auto,
    Half,
    Float16,
    Bfloat16,
    Float,
    Float32,
}

impl VllmDtype {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// vLLM's per-request repetition stop (`RepetitionDetectionParams`, vLLM ≥ 0.17):
/// ends a generation with `finish_reason = "repetition"` once a pattern of
/// `min_pattern_size..=max_pattern_size` tokens comes `min_count` times in a row.
/// Every field is required and nonzero; an older vLLM silently drops the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct VllmRepetitionDetection {
    /// The shortest repeating pattern, in tokens.
    pub min_pattern_size: PositiveCount,
    /// The longest repeating pattern, in tokens. At least `min_pattern_size`.
    pub max_pattern_size: PositiveCount,
    /// How many times in a row a pattern must come. At least two.
    #[cfg_attr(feature = "schemars", schemars(range(min = 2)))]
    pub min_count: PositiveCount,
}

/// `docker run --ipc`, minus `container:<id>`: a cell has no container to join.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    strum::EnumIter,
    strum::IntoStaticStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum DockerIpcMode {
    Host,
    Private,
    Shareable,
    None,
}

impl DockerIpcMode {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// `docker run --shm-size` (`16g`), kept as authored.
#[nutype(
    validate(predicate = is_docker_size),
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
pub struct DockerShmSize(String);

/// Docker's `RAMInBytes` grammar, `<n>[ ][b|<kmgtp>[b|ib]]`, and above zero.
fn is_docker_size(s: &str) -> bool {
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(end);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, "0"));
    let unit = unit.strip_prefix(' ').unwrap_or(unit).to_ascii_lowercase();
    let known_unit = match unit.as_bytes() {
        [] | [b'b'] => true,
        [prefix, rest @ ..] => b"kmgtp".contains(prefix) && matches!(rest, b"" | b"b" | b"ib"),
    };
    [whole, fraction]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && number.bytes().any(|b| matches!(b, b'1'..=b'9'))
        && known_unit
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::runtime::uv::sample_catalog_source;
    use crate::runtime::words_match_serde;
    use crate::*;

    /// `wire` parses to `expected`, or is refused when that is `None`, and emits as itself.
    fn assert_wire<E>(wire: &str, expected: Option<E>) -> anyhow::Result<()>
    where
        E: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert_eq!(
            serde_json::from_value::<E>(wire.into()).ok(),
            expected,
            "{wire:?}"
        );
        if let Some(value) = expected {
            assert_eq!(serde_json::to_value(value)?, wire);
        }
        Ok(())
    }

    #[rstest]
    #[case::nvidia_gpu("nvidia_gpu", Some(VllmFlavor::NvidiaGpu))]
    #[case::amd_gpu("amd_gpu", Some(VllmFlavor::AmdGpu))]
    #[case::cpu("cpu", Some(VllmFlavor::Cpu))]
    #[case::unknown("tpu", None)]
    fn vllm_flavor_wire_names(
        #[case] wire: &str,
        #[case] expected: Option<VllmFlavor>,
    ) -> anyhow::Result<()> {
        assert_wire(wire, expected)
    }

    // A wrong mapping here routes a runtime through the other vendor's GPU
    // preflight, so every branch is pinned.
    #[rstest]
    #[case::cuda("cu121", VllmFlavor::NvidiaGpu)]
    #[case::cpu("cpu", VllmFlavor::Cpu)]
    #[case::rocm("rocm624", VllmFlavor::AmdGpu)]
    fn flavor_from_uv_build_cases(
        #[case] build: &str,
        #[case] expected: VllmFlavor,
    ) -> anyhow::Result<()> {
        let build = UvBuild::try_new(build.to_string())?;
        assert_eq!(flavor_from_uv_build(&build), expected);
        Ok(())
    }

    #[rstest]
    #[case::nvidia_gpu("nvidia_gpu", Some(SglangFlavor::NvidiaGpu))]
    #[case::amd_gpu("amd_gpu", Some(SglangFlavor::AmdGpu))]
    #[case::cpu("cpu", Some(SglangFlavor::Cpu))]
    #[case::unknown("tpu", None)]
    fn sglang_flavor_wire_names(
        #[case] wire: &str,
        #[case] expected: Option<SglangFlavor>,
    ) -> anyhow::Result<()> {
        assert_wire(wire, expected)
    }

    #[rstest]
    #[case::auto("auto", Some(VllmDtype::Auto))]
    #[case::half("half", Some(VllmDtype::Half))]
    #[case::float16("float16", Some(VllmDtype::Float16))]
    #[case::bfloat16("bfloat16", Some(VllmDtype::Bfloat16))]
    #[case::float("float", Some(VllmDtype::Float))]
    #[case::float32("float32", Some(VllmDtype::Float32))]
    #[case::shorthand("bf16", None)]
    fn vllm_dtype_wire_names(
        #[case] wire: &str,
        #[case] expected: Option<VllmDtype>,
    ) -> anyhow::Result<()> {
        assert_wire(wire, expected)
    }

    #[rstest]
    #[case::host("host", Some(DockerIpcMode::Host))]
    #[case::private("private", Some(DockerIpcMode::Private))]
    #[case::shareable("shareable", Some(DockerIpcMode::Shareable))]
    #[case::none("none", Some(DockerIpcMode::None))]
    // A cell has no container to join.
    #[case::container("container:other", None)]
    fn docker_ipc_mode_wire_names(
        #[case] wire: &str,
        #[case] expected: Option<DockerIpcMode>,
    ) -> anyhow::Result<()> {
        assert_wire(wire, expected)
    }

    /// `as_str` reaches the argv, so it must be the word the plan wrote.
    #[test]
    fn argv_words_match_the_wire() -> anyhow::Result<()> {
        words_match_serde(VllmDtype::as_str)?;
        words_match_serde(DockerIpcMode::as_str)
    }

    #[rstest]
    #[case::lower("16g", true)]
    #[case::upper_suffixed("16GB", true)]
    #[case::binary_fraction("1.5GiB", true)]
    #[case::bytes("1073741824", true)]
    #[case::byte_suffix("512b", true)]
    #[case::space_before_unit("16 g", true)]
    #[case::zero("0.0g", false)]
    #[case::unknown_unit("16x", false)]
    #[case::unit_without_b("16gi", false)]
    #[case::binary_without_unit("16ib", false)]
    #[case::bare_fraction(".5g", false)]
    #[case::empty("", false)]
    fn docker_shm_size_grammar(#[case] size: &str, #[case] accepted: bool) {
        match DockerShmSize::try_new(size) {
            Ok(shm) => {
                assert!(accepted, "{size:?} should have been refused");
                assert_eq!(shm.as_ref(), size, "kept as authored");
            }
            Err(e) => assert!(!accepted, "{size:?}: {e}"),
        }
        assert_eq!(
            serde_json::from_value::<DockerShmSize>(size.into()).is_ok(),
            accepted,
            "the plan wire takes the same check"
        );
    }

    #[test]
    fn docker_runtimes_render_their_image_reference() -> anyhow::Result<()> {
        let vllm = DockerVllm {
            image_name: NonEmptyString::try_new("vllm/vllm-openai")?,
            image_tag: NonEmptyString::try_new("v0.20.2")?,
            flavor: VllmFlavor::NvidiaGpu,
        };
        assert_eq!(vllm.to_string(), "docker://vllm/vllm-openai:v0.20.2");
        let sglang = DockerSglang {
            image_name: NonEmptyString::try_new("lmsysorg/sglang")?,
            image_tag: NonEmptyString::try_new("v0.5.15-cu130")?,
            flavor: SglangFlavor::NvidiaGpu,
        };
        assert_eq!(sglang.to_string(), "docker://lmsysorg/sglang:v0.5.15-cu130");
        Ok(())
    }

    #[test]
    fn uv_vllm_display_emits_uv_scheme() -> anyhow::Result<()> {
        let rt = UvVllm {
            server_version: UvServerVersion::try_new("0.10.0".to_owned())?,
            build: UvBuild::try_new("cu121".to_owned())?,
            python_version: UvPythonVersion::try_new("3.12".to_owned())?,
            source: sample_catalog_source()?,
        };
        assert_eq!(rt.runtime_version(), "0.10.0+cu121.py3.12");
        assert_eq!(rt.to_string(), "uv://vllm@0.10.0+cu121.py3.12");
        Ok(())
    }

    #[test]
    fn uv_sglang_display_emits_uv_scheme() -> anyhow::Result<()> {
        let rt = UvSglang {
            server_version: UvServerVersion::try_new("0.4.0".to_owned())?,
            build: UvBuild::try_new("cu121".to_owned())?,
            python_version: UvPythonVersion::try_new("3.12".to_owned())?,
            source: UvRuntimeSource::PipRequirementsText {
                contents: NonEmptyString::try_new("sglang==0.4.0\n".to_owned())?,
                install_flags: None,
            },
        };
        assert_eq!(rt.runtime_version(), "0.4.0+cu121.py3.12");
        assert_eq!(rt.to_string(), "uv://sglang@0.4.0+cu121.py3.12");
        Ok(())
    }
}
