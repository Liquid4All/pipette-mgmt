//! ONNX Runtime GenAI: the uv venv, and the in-process iOS app.

use serde::{Deserialize, Serialize};

use crate::{
    PrivateApis, SourceRepository, UvPythonVersion, UvRuntimeSource, UvServerVersion,
    uv_runtime_version,
};

/// The iOS pipette app's in-process ONNX Runtime GenAI, compiled from source,
/// so its version is the pinned source tree of each submodule. No
/// `execution_provider`: the app builds one CPU engine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OnnxGenaiIosPipette {
    pub packages: OnnxGenaiStack,
    pub flavor: OnnxGenaiIosPipetteFlavor,
    /// As [`LlamacppIosPipette::private_apis`](crate::LlamacppIosPipette::private_apis): they
    /// belong to the app, not the engine.
    #[serde(flatten)]
    pub private_apis: PrivateApis,
}

/// The two source trees the iOS ONNX Runtime GenAI build was compiled from;
/// `onnxruntime-genai` links against `onnxruntime`, so a number depends on both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OnnxGenaiStack {
    pub onnxruntime: SourceRepository,
    pub onnxruntime_genai: SourceRepository,
}

/// Build targets for the ONNX Runtime GenAI iOS app.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum OnnxGenaiIosPipetteFlavor {
    IosArm64,
}

/// `onnxruntime-genai` in a uv venv, pinned to one execution provider. Each
/// provider ships as a separate wheel, so two providers are two venvs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UvOnnxGenai {
    /// The `onnxruntime-genai` version.
    pub server_version: UvServerVersion,
    /// The execution provider this venv installs the wheel for.
    pub execution_provider: OnnxExecutionProvider,
    pub python_version: UvPythonVersion,
    pub source: UvRuntimeSource,
}

/// The ONNX Runtime execution provider a [`UvOnnxGenai`] venv installs for: a
/// named provider with a published wheel, or any other string, kept verbatim.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum OnnxExecutionProvider {
    Cpu,
    Cuda,
    /// DirectML, the Windows GPU provider.
    DirectMl,
    /// Windows ML. No client serves it: GenAI's `append_provider` takes no
    /// `winml` name.
    WinMl,
    /// A provider onnxruntime-genai accepts that pipette does not name.
    #[strum(default, transparent)]
    Custom(String),
}

impl OnnxExecutionProvider {
    /// Every named provider; excludes [`Self::Custom`].
    pub fn known() -> impl Iterator<Item = Self> {
        use strum::IntoEnumIterator as _;
        Self::iter().filter(|provider| !provider.is_custom())
    }

    /// Whether this is a string outside the named set.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    /// Wire form: the canonical name, or the custom string verbatim.
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// Case-insensitive, but a `Custom` keeps the original spelling.
    pub fn parse(s: &str) -> Self {
        s.parse().unwrap_or_else(|_| Self::Custom(s.to_owned()))
    }
}

impl Serialize for OnnxExecutionProvider {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OnnxExecutionProvider {
    /// Refuses `""` rather than folding it into `Custom`: the provider is a
    /// storage-key segment, and an empty one would drop out of the key.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Err(serde::de::Error::custom(
                "execution provider must not be empty",
            ));
        }
        Ok(Self::parse(&s))
    }
}

impl UvOnnxGenai {
    /// `<version>+<ep>.py<python>`: the provider is the build tag.
    pub fn runtime_version(&self) -> String {
        uv_runtime_version(
            &self.server_version,
            &self.execution_provider,
            &self.python_version,
        )
    }
}

/// `uv://onnx-genai@<version>` — wire form for the onnx-genai `--runtime` arg.
impl std::fmt::Display for UvOnnxGenai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uv://onnx-genai@{}", self.runtime_version())
    }
}

impl std::fmt::Display for OnnxGenaiIosPipette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.packages, self.flavor)
    }
}

impl std::fmt::Display for OnnxGenaiStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "onnxruntime={} onnxruntime-genai={}",
            self.onnxruntime, self.onnxruntime_genai
        )
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;
    use crate::runtime::uv::sample_catalog_source;
    use crate::*;

    #[test]
    fn every_known_execution_provider_round_trips() -> anyhow::Result<()> {
        use strum::IntoEnumIterator as _;

        let known: Vec<_> = OnnxExecutionProvider::known().collect();
        assert_eq!(
            known.len(),
            OnnxExecutionProvider::iter().count() - 1,
            "known() must drop exactly `Custom`",
        );
        known.iter().try_for_each(|provider| {
            assert_eq!(&OnnxExecutionProvider::parse(provider.as_str()), provider);
            let wire = serde_json::to_string(provider)?;
            assert_eq!(wire, format!(r#""{provider}""#));
            assert_eq!(
                &serde_json::from_str::<OnnxExecutionProvider>(&wire)?,
                provider
            );
            anyhow::Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn an_unknown_execution_provider_keeps_its_spelling() -> anyhow::Result<()> {
        let custom = OnnxExecutionProvider::parse("QNN-Htp");
        assert_eq!(custom, OnnxExecutionProvider::Custom("QNN-Htp".to_owned()));
        assert!(custom.is_custom());
        assert_eq!(serde_json::to_string(&custom)?, r#""QNN-Htp""#);
        // A known name in any case folds to the named variant.
        assert_eq!(
            OnnxExecutionProvider::parse("CUDA"),
            OnnxExecutionProvider::Cuda
        );
        Ok(())
    }

    #[test]
    fn an_empty_execution_provider_is_refused() -> anyhow::Result<()> {
        let Err(err) = serde_json::from_str::<OnnxExecutionProvider>(r#""""#) else {
            anyhow::bail!("an empty provider should not decode");
        };
        assert!(err.to_string().contains("must not be empty"), "got {err}");
        assert!(serde_json::from_str::<Runtime>(
            r#"{"type":"uv_onnx_genai","server_version":"0.16.0","execution_provider":"",
                "python_version":"3.11",
                "source":{"type":"pip_requirements_text","contents":"onnxruntime-genai==0.16.0\n"}}"#
        )
        .is_err());
        Ok(())
    }

    /// The storage key, a capability flag and the `--runtime` arg all read this
    /// string, so its shape is pinned.
    #[rstest::rstest]
    #[case(OnnxExecutionProvider::Cpu, "0.16.0+cpu.py3.11")]
    #[case(OnnxExecutionProvider::Cuda, "0.16.0+cuda.py3.11")]
    #[case(OnnxExecutionProvider::DirectMl, "0.16.0+directml.py3.11")]
    #[case(OnnxExecutionProvider::Custom("QNN-Htp".to_owned()), "0.16.0+QNN-Htp.py3.11")]
    fn uv_onnx_genai_runtime_version_puts_the_provider_in_the_build_slot(
        #[case] execution_provider: OnnxExecutionProvider,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let rt = UvOnnxGenai {
            server_version: UvServerVersion::try_new("0.16.0".to_owned())?,
            execution_provider,
            python_version: UvPythonVersion::try_new("3.11".to_owned())?,
            source: sample_catalog_source()?,
        };
        assert_eq!(rt.runtime_version(), expected);
        assert_eq!(rt.to_string(), format!("uv://onnx-genai@{expected}"));
        Ok(())
    }

    #[test]
    fn onnx_genai_ios_pipette_runtime_parses() -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(
            r#"type = "onnx_genai_ios_pipette"
flavor = "ios-arm64"

[packages]
onnxruntime = { repository_url = "github.com/microsoft/onnxruntime", repository_version = "v1.23.0" }
onnxruntime_genai = { repository_url = "github.com/microsoft/onnxruntime-genai", repository_version = "v0.15.2" }"#,
        )
        .context("onnx genai ios pipette runtime should parse")?;
        let Runtime::OnnxGenaiIosPipette(rt) = &runtime else {
            anyhow::bail!("expected OnnxGenaiIosPipette, got {runtime:?}");
        };
        assert_eq!(rt.flavor, OnnxGenaiIosPipetteFlavor::IosArm64);
        assert_eq!(
            rt.packages.onnxruntime_genai.repository_version.as_ref(),
            "v0.15.2"
        );
        assert_eq!(
            rt.packages.to_string(),
            "onnxruntime=github.com/microsoft/onnxruntime@v1.23.0 onnxruntime-genai=github.com/microsoft/onnxruntime-genai@v0.15.2"
        );
        assert_eq!(rt.to_string(), format!("{}:ios-arm64", rt.packages));
        assert_eq!(rt.private_apis, PrivateApis::default());
        Ok(())
    }
}
