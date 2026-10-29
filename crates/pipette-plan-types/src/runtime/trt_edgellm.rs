//! NVIDIA TensorRT Edge-LLM: a server archive driven over llama-server's HTTP
//! dialect, building its engines on the device it runs on.

use serde::{Deserialize, Serialize};

use crate::{
    AbsolutePath, NonEmptyTrimmedString, RelativePath, RemoteArchiveUrl, VaultPath,
    VaultPrincipalId,
};

/// A TensorRT Edge-LLM server archive, one per SoC and CUDA major (see
/// [`TrtEdgellmFlavor`]). It serves a GGUF vision pair, building TensorRT
/// engines from it into the client's compile cache before the measured
/// windows. The engines also depend on the host's TensorRT, which the server
/// keys its cache by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct TrtEdgellm {
    #[serde(flatten)]
    pub source: TrtEdgellmSource,
    pub flavor: TrtEdgellmFlavor,
}

/// Where a TensorRT Edge-LLM archive comes from: [`OvmsSource`](crate::OvmsSource)'s
/// arms and wire tags, less `github_release`, since upstream publishes no
/// server archive. `deny_unknown_fields` lives here because `flatten` bars it
/// from [`TrtEdgellm`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrtEdgellmSource {
    RemoteArchive {
        url: RemoteArchiveUrl,
    },
    VaultRemoteArchive {
        url: VaultPath,
        principal_id: VaultPrincipalId,
        /// The vault record's `name`, which ingestion checks: a path says only an id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
        custom_label: Option<NonEmptyTrimmedString>,
    },
    RelativeDir {
        dir: RelativePath,
    },
    AbsoluteDir {
        dir: AbsolutePath,
    },
}

/// The SoC and CUDA major an archive's plugin is built for. Kebab-case on the
/// wire; `Custom` names a private build, emitted verbatim.
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
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum TrtEdgellmFlavor {
    /// A Jetson Orin (`nvidia,tegra234`) on CUDA 13, as JetPack 7.2 ships.
    #[strum(serialize = "jetson-orin-cuda-13")]
    JetsonOrinCuda13,
    /// A build outside the named set.
    #[strum(default, transparent)]
    Custom(String),
}

impl TrtEdgellmFlavor {
    /// Every flavor pipette names, for a listing or a validator. Excludes
    /// [`Self::Custom`], which is open by construction.
    pub fn known() -> impl Iterator<Item = Self> {
        use strum::IntoEnumIterator as _;
        Self::iter().filter(|flavor| !flavor.is_custom())
    }

    /// Whether this is a string outside the named set; [`Self::parse`] never fails.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    /// Wire form: the canonical name, or the custom string verbatim.
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// Parse a wire form. Case-insensitive on the named set; a `Custom` keeps
    /// the original spelling.
    pub fn parse(s: &str) -> Self {
        s.parse().unwrap_or_else(|_| Self::Custom(s.to_owned()))
    }
}

impl Serialize for TrtEdgellmFlavor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TrtEdgellmFlavor {
    /// Held to slug characters, non-empty, because it is spliced into the
    /// runtime URI and the storage key.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let is_slug = !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !is_slug {
            return Err(serde::de::Error::custom(format!(
                "trt_edgellm flavor must be non-empty [A-Za-z0-9._-], got {s:?}"
            )));
        }
        Ok(Self::parse(&s))
    }
}

/// The language model's engine precision. The vision encoder builds at FP16
/// whatever this says. Closed so a typo fails at plan parse, not after an
/// engine build.
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
pub enum TrtEdgellmPrecision {
    Fp16,
    /// The GEMMs and norms in BF16; attention, conv and the KV cache stay FP16.
    Bf16,
    /// INT8 weights per output channel with FP16 activations, quantized at
    /// engine build from the BF16 GGUF; not Q8_0's block scales.
    W8a16,
}

impl TrtEdgellmPrecision {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// As [`OvmsSource`](crate::OvmsSource)'s.
impl std::fmt::Display for TrtEdgellmSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrtEdgellmSource::RemoteArchive { url } => write!(f, "{url}"),
            TrtEdgellmSource::VaultRemoteArchive { url, .. } => write!(f, "{url}"),
            TrtEdgellmSource::RelativeDir { dir } => write!(f, "relative-dir:{dir}"),
            TrtEdgellmSource::AbsoluteDir { dir } => write!(f, "absolute-dir:{dir}"),
        }
    }
}

/// `{source}:{flavor}`, as [`Ovms`](crate::Ovms).
impl std::fmt::Display for TrtEdgellm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.flavor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn a_flavor_outside_the_set_keeps_its_spelling() -> anyhow::Result<()> {
        let custom: TrtEdgellmFlavor = serde_json::from_str(r#""JetPack6-Orin""#)?;
        assert_eq!(custom, TrtEdgellmFlavor::Custom("JetPack6-Orin".to_owned()));
        assert_eq!(serde_json::to_string(&custom)?, r#""JetPack6-Orin""#);
        Ok(())
    }

    #[test]
    fn precision_words_match_serde() -> anyhow::Result<()> {
        crate::runtime::words_match_serde(TrtEdgellmPrecision::as_str)
    }

    /// Every arm round-trips and renders `{source}:{flavor}`, which is also
    /// the binary's ref.
    #[rstest::rstest]
    #[case::remote_archive(
        r#"{"type":"trt_edgellm","source":"remote_archive","url":"https://mirror.example/trt.tar.gz","flavor":"jetson-orin-cuda-13"}"#,
        "https://mirror.example/trt.tar.gz:jetson-orin-cuda-13"
    )]
    #[case::vault_remote_archive(
        r#"{"type":"trt_edgellm","source":"vault_remote_archive","url":"/vault/resources/vault_1/content","principal_id":"org_01936d","custom_label":"TensorRT Edge-LLM 0.11.0+lfm3","flavor":"jetson-orin-cuda-13"}"#,
        "/vault/resources/vault_1/content:jetson-orin-cuda-13"
    )]
    #[case::relative_dir(
        r#"{"type":"trt_edgellm","source":"relative_dir","dir":"blobs","flavor":"jetson-orin-cuda-13"}"#,
        "relative-dir:blobs:jetson-orin-cuda-13"
    )]
    #[case::absolute_dir(
        r#"{"type":"trt_edgellm","source":"absolute_dir","dir":"/opt/trt","flavor":"jetson-orin-cuda-13"}"#,
        "absolute-dir:/opt/trt:jetson-orin-cuda-13"
    )]
    fn every_source_arm_round_trips(
        #[case] json: &str,
        #[case] display: &str,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = serde_json::from_str(json)?;
        assert_eq!(runtime.to_string(), display);
        assert_eq!(runtime.cli_ref(), display);
        let back: Runtime = serde_json::from_str(&serde_json::to_string(&runtime)?)?;
        assert_eq!(runtime, back);
        Ok(())
    }

    /// Refused at parse; `deny_unknown_fields` bites through `flatten`.
    #[rstest::rstest]
    #[case::no_upstream_release(
        r#"{"type":"trt_edgellm","source":"github_release","repository_url":"github.com/NVIDIA/TensorRT-Edge-LLM","repository_version":"v0.11.0","flavor":"jetson-orin-cuda-13"}"#,
        "unknown variant `github_release`"
    )]
    #[case::precision_on_the_runtime(
        r#"{"type":"trt_edgellm","source":"relative_dir","dir":"blobs","flavor":"jetson-orin-cuda-13","precision":"fp16"}"#,
        "unknown field `precision`"
    )]
    #[case::blank_flavor(
        r#"{"type":"trt_edgellm","source":"relative_dir","dir":"blobs","flavor":""}"#,
        "trt_edgellm flavor"
    )]
    #[case::flavor_with_a_slash(
        r#"{"type":"trt_edgellm","source":"relative_dir","dir":"blobs","flavor":"jetpack/7"}"#,
        "trt_edgellm flavor"
    )]
    fn a_malformed_runtime_is_refused(
        #[case] json: &str,
        #[case] reason: &str,
    ) -> anyhow::Result<()> {
        let Err(err) = serde_json::from_str::<Runtime>(json) else {
            anyhow::bail!("should not decode: {json}");
        };
        assert!(err.to_string().contains(reason), "{json}: {err}");
        Ok(())
    }
}
