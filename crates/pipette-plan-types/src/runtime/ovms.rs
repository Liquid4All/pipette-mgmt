//! OpenVINO Model Server: a release archive driven over HTTP.

use serde::{Deserialize, Serialize};

use crate::{
    AbsolutePath, NonEmptyString, NonEmptyTrimmedString, RelativePath, RemoteArchiveUrl,
    RepositoryUrl, VaultPath, VaultPrincipalId,
};

/// An OpenVINO Model Server (OVMS) release archive: a native binary that
/// bundles its own OpenVINO, so there is no venv and no Python. The device is a
/// per-cell flag, as on [`UvOpenvino`](crate::UvOpenvino): one archive serves
/// CPU, GPU and NPU.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Ovms {
    #[serde(flatten)]
    pub source: OvmsSource,
    pub flavor: OvmsFlavor,
}

/// Where an OVMS archive comes from: [`LlamacppCliStockToolsSource`](crate::LlamacppCliStockToolsSource)'s arms and
/// wire tags.
/// `deny_unknown_fields` lives here because `flatten` bars it from [`Ovms`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum OvmsSource {
    /// An upstream release asset, named by [`OvmsFlavor::release_asset_name`].
    GithubRelease(OvmsSourceRepository),
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

/// [`SourceRepository`](crate::SourceRepository)'s shape, but rejecting unknown keys.
/// `repository_version` is the release tag verbatim (`v2026.4.0`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OvmsSourceRepository {
    pub repository_url: RepositoryUrl,
    #[serde(alias = "version")]
    pub repository_version: NonEmptyString,
}

/// The OVMS release archive a host installs, one per OS (see
/// [`Self::release_asset_name`]). `python_off` is implied: the cells call
/// `/v3/completions`, which applies no chat template. Kebab-case on the wire;
/// `Custom` names a private build, emitted verbatim.
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
pub enum OvmsFlavor {
    WindowsX64,
    Ubuntu22X64,
    Ubuntu24X64,
    RedhatX64,
    /// A build outside the upstream release set.
    #[strum(default, transparent)]
    Custom(String),
}

impl OvmsFlavor {
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

    /// The `openvinotoolkit/model_server` asset for this flavor at release `tag`;
    /// `None` for [`Self::Custom`] and before `v2026.1`, whose names omit the
    /// version. The asset pads the tag to three parts: `v2026.3` ships `…_2026.3.0_…`.
    pub fn release_asset_name(&self, tag: &str) -> Option<String> {
        let (os, ext) = match self {
            Self::WindowsX64 => ("windows", "zip"),
            Self::Ubuntu22X64 => ("ubuntu22", "tar.gz"),
            Self::Ubuntu24X64 => ("ubuntu24", "tar.gz"),
            Self::RedhatX64 => ("redhat", "tar.gz"),
            Self::Custom(_) => return None,
        };
        let version = tag.strip_prefix('v')?;
        let parts: Vec<&str> = version.split('.').collect();
        if parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let (major, minor, asset_version) = match parts.as_slice() {
            [major, minor] => (major, minor, format!("{version}.0")),
            [major, minor, _] => (major, minor, version.to_owned()),
            _ => return None,
        };
        if (major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?) < (2026, 1) {
            return None;
        }
        Some(format!("ovms_{os}_{asset_version}_python_off.{ext}"))
    }

    /// Parse a wire form. Case-insensitive on the named set; a `Custom` keeps
    /// the original spelling.
    pub fn parse(s: &str) -> Self {
        s.parse().unwrap_or_else(|_| Self::Custom(s.to_owned()))
    }
}

impl Serialize for OvmsFlavor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OvmsFlavor {
    /// Held to slug characters, non-empty, because it is spliced into the
    /// runtime URI and the storage key.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let is_slug = !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !is_slug {
            return Err(serde::de::Error::custom(format!(
                "ovms flavor must be non-empty [A-Za-z0-9._-], got {s:?}"
            )));
        }
        Ok(Self::parse(&s))
    }
}

/// As [`LlamacppCliStockToolsSource`](crate::LlamacppCliStockToolsSource)'s.
impl std::fmt::Display for OvmsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OvmsSource::GithubRelease(repo) => write!(f, "{repo}"),
            OvmsSource::RemoteArchive { url } => write!(f, "{url}"),
            OvmsSource::VaultRemoteArchive { url, .. } => write!(f, "{url}"),
            OvmsSource::RelativeDir { dir } => write!(f, "relative-dir:{dir}"),
            OvmsSource::AbsoluteDir { dir } => write!(f, "absolute-dir:{dir}"),
        }
    }
}

/// `<repository_url>@<repository_version>`, as [`SourceRepository`](crate::SourceRepository).
impl std::fmt::Display for OvmsSourceRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.repository_url, self.repository_version)
    }
}

/// `{source}:{flavor}`, as [`LlamacppCliStockTools`](crate::LlamacppCliStockTools).
impl std::fmt::Display for Ovms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.flavor)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;
    use crate::*;

    /// Each named flavor is the one spelling both directions use, so a
    /// catalog row, a plan and a stored descriptor agree on it.
    #[rstest::rstest]
    #[case(OvmsFlavor::WindowsX64, "windows-x64")]
    #[case(OvmsFlavor::Ubuntu22X64, "ubuntu22-x64")]
    #[case(OvmsFlavor::Ubuntu24X64, "ubuntu24-x64")]
    #[case(OvmsFlavor::RedhatX64, "redhat-x64")]
    fn ovms_flavor_wire_names(
        #[case] flavor: OvmsFlavor,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(serde_json::to_string(&flavor)?, format!(r#""{wire}""#));
        assert_eq!(
            serde_json::from_str::<OvmsFlavor>(&format!(r#""{wire}""#))?,
            flavor
        );
        assert_eq!(OvmsFlavor::parse(&wire.to_ascii_uppercase()), flavor);
        assert_eq!(flavor.as_str(), wire);
        Ok(())
    }

    #[test]
    fn ovms_known_flavors_exclude_the_escape_hatch() {
        use strum::IntoEnumIterator as _;
        assert_eq!(OvmsFlavor::known().count(), OvmsFlavor::iter().count() - 1);
        assert!(OvmsFlavor::known().all(|flavor| !flavor.is_custom()));
    }

    /// A private build keeps its spelling.
    #[test]
    fn an_ovms_flavor_outside_the_set_keeps_its_spelling() -> anyhow::Result<()> {
        let custom: OvmsFlavor = serde_json::from_str(r#""Acme-Win-Arm64""#)?;
        assert_eq!(custom, OvmsFlavor::Custom("Acme-Win-Arm64".to_owned()));
        assert!(custom.is_custom());
        assert_eq!(custom.as_str(), "Acme-Win-Arm64");
        assert_eq!(serde_json::to_string(&custom)?, r#""Acme-Win-Arm64""#);
        Ok(())
    }

    /// Upstream's `python_off` asset per release, one column per named flavor
    /// (windows, ubuntu22, ubuntu24, redhat), copied from
    /// `gh api repos/openvinotoolkit/model_server/releases` on 2026-09-24.
    const OVMS_UPSTREAM_ASSETS: &[(&str, [&str; 4])] = &[
        (
            "v2026.4.0",
            [
                "ovms_windows_2026.4.0_python_off.zip",
                "ovms_ubuntu22_2026.4.0_python_off.tar.gz",
                "ovms_ubuntu24_2026.4.0_python_off.tar.gz",
                "ovms_redhat_2026.4.0_python_off.tar.gz",
            ],
        ),
        (
            "v2026.3.1",
            [
                "ovms_windows_2026.3.1_python_off.zip",
                "ovms_ubuntu22_2026.3.1_python_off.tar.gz",
                "ovms_ubuntu24_2026.3.1_python_off.tar.gz",
                "ovms_redhat_2026.3.1_python_off.tar.gz",
            ],
        ),
        (
            "v2026.3",
            [
                "ovms_windows_2026.3.0_python_off.zip",
                "ovms_ubuntu22_2026.3.0_python_off.tar.gz",
                "ovms_ubuntu24_2026.3.0_python_off.tar.gz",
                "ovms_redhat_2026.3.0_python_off.tar.gz",
            ],
        ),
        (
            "v2026.2.1",
            [
                "ovms_windows_2026.2.1_python_off.zip",
                "ovms_ubuntu22_2026.2.1_python_off.tar.gz",
                "ovms_ubuntu24_2026.2.1_python_off.tar.gz",
                "ovms_redhat_2026.2.1_python_off.tar.gz",
            ],
        ),
        (
            "v2026.2",
            [
                "ovms_windows_2026.2.0_python_off.zip",
                "ovms_ubuntu22_2026.2.0_python_off.tar.gz",
                "ovms_ubuntu24_2026.2.0_python_off.tar.gz",
                "ovms_redhat_2026.2.0_python_off.tar.gz",
            ],
        ),
        (
            "v2026.1",
            [
                "ovms_windows_2026.1.0_python_off.zip",
                "ovms_ubuntu22_2026.1.0_python_off.tar.gz",
                "ovms_ubuntu24_2026.1.0_python_off.tar.gz",
                "ovms_redhat_2026.1.0_python_off.tar.gz",
            ],
        ),
    ];

    const OVMS_ASSET_COLUMNS: [OvmsFlavor; 4] = [
        OvmsFlavor::WindowsX64,
        OvmsFlavor::Ubuntu22X64,
        OvmsFlavor::Ubuntu24X64,
        OvmsFlavor::RedhatX64,
    ];

    #[test]
    fn ovms_release_asset_names_match_upstream() {
        for (tag, assets) in OVMS_UPSTREAM_ASSETS {
            for (flavor, asset) in OVMS_ASSET_COLUMNS.iter().zip(assets) {
                assert_eq!(
                    flavor.release_asset_name(tag).as_deref(),
                    Some(*asset),
                    "{tag} {flavor}"
                );
            }
        }
    }

    /// The table names every flavor pipette names, so a new one fails here
    /// until its upstream asset is pinned.
    #[test]
    fn ovms_asset_columns_cover_every_known_flavor() {
        assert_eq!(
            OvmsFlavor::known().collect::<Vec<_>>(),
            OVMS_ASSET_COLUMNS.to_vec()
        );
    }

    /// Tags whose upstream names the rule does not produce, so no URL is
    /// guessed. `v2026.0` ships `ovms_windows_python_off.zip`; `v2025.4` ships
    /// `ovms_ubuntu22.tar.gz`.
    #[rstest::rstest]
    #[case::before_versioned_names("v2026.0")]
    #[case::before_python_off_on_linux("v2025.4")]
    #[case::no_v_prefix("2026.4.0")]
    #[case::prerelease("v2026.4.0-rc1")]
    #[case::one_part("v2026")]
    #[case::four_parts("v2026.4.0.1")]
    #[case::signed_part("v2026.+4")]
    fn an_ovms_tag_outside_the_rule_names_no_asset(#[case] tag: &str) {
        for flavor in OvmsFlavor::known() {
            assert_eq!(flavor.release_asset_name(tag), None, "{tag} {flavor}");
        }
    }

    #[test]
    fn a_custom_ovms_flavor_names_no_asset() {
        assert_eq!(
            OvmsFlavor::Custom("acme-x64".to_owned()).release_asset_name("v2026.4.0"),
            None
        );
    }

    /// A `github_release` carries llama.cpp's keys, so a plan author spells both
    /// the same way.
    #[test]
    fn ovms_github_release_has_the_llamacpp_shape() -> anyhow::Result<()> {
        fn keys(runtime: &Runtime) -> anyhow::Result<Vec<String>> {
            let value = serde_json::to_value(runtime)?;
            let object = value.as_object().context("runtime is an object")?;
            Ok(object.keys().cloned().collect())
        }
        let ovms: Runtime = toml::from_str(
            r#"type = "ovms"
source = "github_release"
repository_url = "github.com/openvinotoolkit/model_server"
version = "v2026.4.0"
flavor = "windows-x64""#,
        )?;
        let llama: Runtime = toml::from_str(
            r#"type = "llamacpp_cli_stock_tools"
source = "github_release"
repository_url = "github.com/ggml-org/llama.cpp"
version = "b9305"
flavor = "macos-arm64""#,
        )?;
        assert_eq!(keys(&ovms)?, keys(&llama)?);
        // The descriptor `docs/storage.md` shows.
        assert_eq!(
            serde_json::to_value(&ovms)?,
            serde_json::from_str::<serde_json::Value>(
                r#"{"flavor":"windows-x64","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","source":"github_release","type":"ovms"}"#
            )?
        );
        assert_eq!(
            ovms.to_string(),
            "github.com/openvinotoolkit/model_server@v2026.4.0:windows-x64"
        );
        assert_eq!(ovms.cli_ref(), ovms.to_string());
        Ok(())
    }

    /// The repo is part of the canonical id, so a fork stays distinct from
    /// upstream.
    #[rstest::rstest]
    #[case::upstream(
        r#"{"source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","version":"v2026.4.0","flavor":"windows-x64"}"#,
        RepositoryUrl::new("github.com/openvinotoolkit/model_server"),
        "github.com/openvinotoolkit/model_server@v2026.4.0:windows-x64"
    )]
    #[case::fork(
        r#"{"source":"github_release","repository_url":"https://github.com/acme/model_server.git","repository_version":"v2026.4.0","flavor":"windows-x64"}"#,
        RepositoryUrl::new("github.com/acme/model_server"),
        "github.com/acme/model_server@v2026.4.0:windows-x64"
    )]
    fn an_ovms_github_release_names_its_repository(
        #[case] json: &str,
        #[case] repository_url: RepositoryUrl,
        #[case] display: &str,
    ) -> anyhow::Result<()> {
        let ovms: Ovms = serde_json::from_str(json)?;
        assert_eq!(
            ovms,
            Ovms {
                source: OvmsSource::GithubRelease(OvmsSourceRepository {
                    repository_url,
                    repository_version: NonEmptyString::try_new("v2026.4.0")?,
                }),
                flavor: OvmsFlavor::WindowsX64,
            }
        );
        assert_eq!(ovms.to_string(), display);
        Ok(())
    }

    /// Every other arm parses under llama.cpp's spelling and round-trips.
    #[rstest::rstest]
    #[case::remote_archive(
        r#"{"type":"ovms","source":"remote_archive","url":"https://mirror.example/ovms.zip","flavor":"windows-x64"}"#,
        "https://mirror.example/ovms.zip:windows-x64"
    )]
    #[case::vault_remote_archive(
        r#"{"type":"ovms","source":"vault_remote_archive","url":"/vault/resources/vault_1/content","principal_id":"org_01936d","flavor":"acme-x64"}"#,
        "/vault/resources/vault_1/content:acme-x64"
    )]
    #[case::labelled_vault_remote_archive(
        r#"{"type":"ovms","source":"vault_remote_archive","url":"/vault/resources/vault_1/content","principal_id":"org_01936d","custom_label":"acme OVMS build","flavor":"acme-x64"}"#,
        "/vault/resources/vault_1/content:acme-x64"
    )]
    #[case::relative_dir(
        r#"{"type":"ovms","source":"relative_dir","dir":"blobs","flavor":"ubuntu24-x64"}"#,
        "relative-dir:blobs:ubuntu24-x64"
    )]
    #[case::absolute_dir(
        r#"{"type":"ovms","source":"absolute_dir","dir":"/opt/ovms","flavor":"redhat-x64"}"#,
        "absolute-dir:/opt/ovms:redhat-x64"
    )]
    fn every_ovms_source_arm_round_trips(
        #[case] json: &str,
        #[case] display: &str,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = serde_json::from_str(json)?;
        assert_eq!(runtime.to_string(), display);
        let back: Runtime = serde_json::from_str(&serde_json::to_string(&runtime)?)?;
        assert_eq!(runtime, back);
        Ok(())
    }

    /// Refused at parse; `deny_unknown_fields` bites through `flatten`.
    #[rstest::rstest]
    #[case::device_on_the_runtime(
        r#"{"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","flavor":"windows-x64","device":"gpu"}"#,
        "unknown field `device`"
    )]
    #[case::python_version(
        r#"{"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","flavor":"windows-x64","python_version":"3.11"}"#,
        "unknown field `python_version`"
    )]
    #[case::unknown_key_on_an_archive(
        r#"{"type":"ovms","source":"remote_archive","url":"https://mirror.example/ovms.zip","sha":"x","flavor":"windows-x64"}"#,
        "unknown field `sha`"
    )]
    #[case::server_version_instead_of_a_source(
        r#"{"type":"ovms","server_version":"2026.4.0","flavor":"windows-x64"}"#,
        "missing field `source`"
    )]
    #[case::no_repository_url(
        r#"{"type":"ovms","source":"github_release","repository_version":"v2026.4.0","flavor":"windows-x64"}"#,
        "missing field `repository_url`"
    )]
    #[case::blank_tag(
        r#"{"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"","flavor":"windows-x64"}"#,
        "NonEmptyString"
    )]
    #[case::blank_flavor(
        r#"{"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","flavor":""}"#,
        "ovms flavor"
    )]
    #[case::flavor_with_a_separator(
        r#"{"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","flavor":"acme&version=9"}"#,
        "ovms flavor"
    )]
    #[case::flavor_with_a_slash(
        r#"{"type":"ovms","source":"github_release","repository_url":"github.com/openvinotoolkit/model_server","repository_version":"v2026.4.0","flavor":"acme/arm64"}"#,
        "ovms flavor"
    )]
    fn a_malformed_ovms_runtime_is_refused(
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
