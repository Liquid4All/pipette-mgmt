//! OpenVINO GenAI in a uv venv, and the compute device an OpenVINO cell runs on.

use serde::{Deserialize, Serialize};

use crate::{UvPythonVersion, UvRuntimeSource, UvServerVersion};

/// `openvino-genai` in a uv venv. The compute device is a per-cell flag, not a
/// field here: one wheel serves CPU, GPU and NPU, so they share one venv.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UvOpenvino {
    /// The `openvino-genai` version, which must match the `openvino` version
    /// that produced the IR.
    pub server_version: UvServerVersion,
    pub python_version: UvPythonVersion,
    pub source: UvRuntimeSource,
}

/// The OpenVINO compute device a cell runs on: a named device, or any other
/// string OpenVINO accepts (`GPU.1`, `HETERO:GPU,CPU`), kept verbatim because
/// device names are case-sensitive. A `Custom` device never gets the NPU rules,
/// even when its string mentions one.
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
pub enum OpenvinoDevice {
    Cpu,
    Gpu,
    /// Intel AI Boost. Static-shape only — see `docs/openvino-ir.md`.
    Npu,
    /// A device OpenVINO accepts that pipette does not name.
    #[strum(default, transparent)]
    Custom(String),
}

impl OpenvinoDevice {
    /// Every named device; excludes [`Self::Custom`].
    pub fn known() -> impl Iterator<Item = Self> {
        use strum::IntoEnumIterator as _;
        Self::iter().filter(|device| !device.is_custom())
    }

    /// Whether this is a string outside the named set.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    /// Wire form: the canonical name, or the custom string verbatim.
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// Case-insensitive, but a `Custom` keeps the original spelling: OpenVINO
    /// resolves `GPU.1`, not `gpu.1`.
    pub fn parse(s: &str) -> Self {
        s.parse().unwrap_or_else(|_| Self::Custom(s.to_owned()))
    }
}

impl Serialize for OpenvinoDevice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OpenvinoDevice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

impl UvOpenvino {
    /// `<version>.py<python>`: no build tag, since one wheel serves every device.
    pub fn runtime_version(&self) -> String {
        format!("{}.py{}", self.server_version, self.python_version)
    }
}

/// `uv://openvino@<version>` — wire form for the openvino `--runtime` arg.
impl std::fmt::Display for UvOpenvino {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uv://openvino@{}", self.runtime_version())
    }
}

/// OpenVINO's NPU compile/performance tradeoff (`GENERATE_HINT`). Closed so a
/// bad value fails at plan parse, not at device compile after the downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum OpenvinoGenerateHint {
    BestPerf,
    FastCompile,
}

impl OpenvinoGenerateHint {
    /// The exact `GENERATE_HINT` value OpenVINO accepts.
    pub fn as_property(self) -> &'static str {
        match self {
            Self::BestPerf => "BEST_PERF",
            Self::FastCompile => "FAST_COMPILE",
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::runtime::uv::sample_catalog_source;
    use crate::*;

    /// A `device` on the runtime is refused, not silently dropped, so a cell
    /// never reaches the client naming no device.
    #[test]
    fn a_device_on_the_runtime_is_refused() -> anyhow::Result<()> {
        let json = serde_json::json!({
            "type": "uv_openvino",
            "server_version": "2026.2.1",
            "device": "npu",
            "python_version": "3.11",
            "source": { "type": "pip_requirements_text", "contents": "openvino-genai==2026.2.1.0\n" },
        });
        let Err(err) = serde_json::from_value::<Runtime>(json) else {
            anyhow::bail!("expected a pre-move runtime to be refused");
        };
        assert!(err.to_string().contains("device"), "got {err}");
        Ok(())
    }

    #[rstest]
    #[case::named_lowercase("npu", OpenvinoDevice::Npu, false)]
    #[case::named_uppercase("NPU", OpenvinoDevice::Npu, false)]
    #[case::indexed_gpu("GPU.1", OpenvinoDevice::Custom("GPU.1".to_owned()), true)]
    #[case::virtual_device(
        "HETERO:GPU,CPU",
        OpenvinoDevice::Custom("HETERO:GPU,CPU".to_owned()),
        true
    )]
    fn openvino_device_parses_named_and_custom(
        #[case] wire: &str,
        #[case] expected: OpenvinoDevice,
        #[case] custom: bool,
    ) {
        let device = OpenvinoDevice::parse(wire);
        assert_eq!(device, expected);
        assert_eq!(device.is_custom(), custom);
    }

    #[rstest]
    #[case("cpu")]
    #[case("gpu")]
    #[case("npu")]
    #[case("GPU.1")]
    #[case("AUTO")]
    fn openvino_device_round_trips(#[case] wire: &str) -> anyhow::Result<()> {
        assert_eq!(OpenvinoDevice::parse(wire).as_str(), wire);
        let device: OpenvinoDevice = serde_json::from_value(wire.into())?;
        assert_eq!(serde_json::to_value(&device)?, wire);
        Ok(())
    }

    #[test]
    fn openvino_known_devices_exclude_the_escape_hatch() {
        let known: Vec<String> = OpenvinoDevice::known().map(|d| d.to_string()).collect();
        assert_eq!(known, vec!["cpu", "gpu", "npu"]);
        assert!(OpenvinoDevice::known().all(|d| OpenvinoDevice::parse(d.as_str()) == d));
    }

    #[test]
    fn uv_openvino_runtime_version_has_no_build_tag() -> anyhow::Result<()> {
        let rt = UvOpenvino {
            server_version: UvServerVersion::try_new("2026.2.1")?,
            python_version: UvPythonVersion::try_new("3.11")?,
            source: sample_catalog_source()?,
        };
        assert_eq!(rt.runtime_version(), "2026.2.1.py3.11");
        assert_eq!(rt.to_string(), "uv://openvino@2026.2.1.py3.11");
        Ok(())
    }

    #[rstest]
    #[case::best_perf("best-perf", Some(OpenvinoGenerateHint::BestPerf))]
    #[case::fast_compile("fast-compile", Some(OpenvinoGenerateHint::FastCompile))]
    #[case::property_spelling("BEST_PERF", None)]
    #[case::snake_case("best_perf", None)]
    #[case::unknown("turbo", None)]
    fn generate_hint_is_a_closed_vocabulary(
        #[case] wire: &str,
        #[case] expected: Option<OpenvinoGenerateHint>,
    ) -> anyhow::Result<()> {
        assert_eq!(
            serde_json::from_value::<OpenvinoGenerateHint>(wire.into()).ok(),
            expected
        );
        if let Some(hint) = expected {
            assert_eq!(serde_json::to_value(hint)?, wire);
        }
        Ok(())
    }

    #[rstest]
    #[case::best_perf(OpenvinoGenerateHint::BestPerf, "BEST_PERF")]
    #[case::fast_compile(OpenvinoGenerateHint::FastCompile, "FAST_COMPILE")]
    fn generate_hint_renders_the_property_openvino_accepts(
        #[case] hint: OpenvinoGenerateHint,
        #[case] property: &str,
    ) {
        assert_eq!(hint.as_property(), property);
    }
}
