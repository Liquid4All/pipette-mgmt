//! llama.cpp: the stock CLI tools, and the in-process Android and iOS apps.

use serde::{Deserialize, Serialize};

use crate::{
    AbsolutePath, FileName, NonEmptyString, NonEmptyTrimmedString, PrivateApis, RelativePath,
    RemoteArchiveUrl, RepositoryUrl, SourceRepository, VaultPath, VaultPrincipalId,
};

/// Where the llama.cpp CLI runtime's *prebuilt* binaries come from; there is no
/// git build. A GitHub release (`version` aliases `repository_version`)
/// resolves to `https://<repo>/releases/download/<tag>/<asset>`. Plans author
/// only the fetching sources (`github_release`, `remote_archive`,
/// `vault_remote_archive`), and URIs spell the first two; `relative_dir` and
/// `absolute_dir` are the installed form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum LlamacppCliStockToolsSource {
    GithubRelease(LlamacppCliStockToolsRelease),
    /// Prebuilt archive at an `http(s)://` URL. See [`RemoteArchiveUrl`].
    RemoteArchive {
        url: RemoteArchiveUrl,
    },
    /// The same archive served by mgmt to one principal. See [`VaultPath`].
    VaultRemoteArchive {
        url: VaultPath,
        principal_id: VaultPrincipalId,
        /// The vault record's `name`, which ingestion checks: a path says only an id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
        custom_label: Option<NonEmptyTrimmedString>,
    },
    /// Portable install layout under the store entry (e.g. `blobs`).
    RelativeDir {
        dir: RelativePath,
    },
    /// Absolute install root on the host after bind.
    AbsoluteDir {
        dir: AbsolutePath,
    },
}

impl LlamacppCliStockToolsSource {
    /// The ref-ish token used in the `--runtime` display form and identity
    /// markers.
    pub fn reference(&self) -> &str {
        match self {
            LlamacppCliStockToolsSource::GithubRelease(repo) => repo.repository_version.as_ref(),
            LlamacppCliStockToolsSource::RemoteArchive { url } => url.as_ref(),
            // The id alone: the route around it is the same for every resource.
            LlamacppCliStockToolsSource::VaultRemoteArchive { url, .. } => {
                url.resource_id().as_ref()
            }
            LlamacppCliStockToolsSource::RelativeDir { dir } => dir.as_ref(),
            LlamacppCliStockToolsSource::AbsoluteDir { dir } => dir.as_ref(),
        }
    }

    /// The submission `runtime_name`.
    pub fn runtime_name(&self) -> String {
        match self {
            LlamacppCliStockToolsSource::GithubRelease(repo) => repo.repository_url.to_string(),
            LlamacppCliStockToolsSource::RemoteArchive { url } => url.to_string(),
            LlamacppCliStockToolsSource::VaultRemoteArchive { url, .. } => url.to_string(),
            LlamacppCliStockToolsSource::RelativeDir { dir } => dir.to_string(),
            LlamacppCliStockToolsSource::AbsoluteDir { dir } => dir.to_string(),
        }
    }

    /// A coarse, stable token for the build's origin, used as the leading
    /// segment of the eval-checkpoint marker.
    pub fn origin_slug(&self) -> &str {
        match self {
            LlamacppCliStockToolsSource::GithubRelease(repo) => repo.repository_url.org_repo(),
            LlamacppCliStockToolsSource::RemoteArchive { .. } => "remote-archive",
            // Distinct from its public twin: the bytes came from mgmt, a
            // different origin even when `url` names the same host.
            LlamacppCliStockToolsSource::VaultRemoteArchive { .. } => "vault-remote-archive",
            LlamacppCliStockToolsSource::RelativeDir { .. } => "local",
            LlamacppCliStockToolsSource::AbsoluteDir { .. } => "local",
        }
    }
}

/// A GitHub release of llama.cpp: [`SourceRepository`]'s two fields, and the
/// asset to download from it when not the one the flavor names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct LlamacppCliStockToolsRelease {
    #[serde(alias = "source_repo")]
    pub repository_url: RepositoryUrl,
    #[serde(alias = "version")]
    pub repository_version: NonEmptyString,
    /// Downloaded instead of [`LlamaCppFlavor::release_asset_name`]'s, for a
    /// file the flavor table doesn't name (a fork's, or a rename newer than
    /// this crate). The flavor still says what it runs on. Omitted when unset,
    /// so a release that names none keeps its descriptor digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<FileName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct LlamacppCliStockTools {
    #[serde(flatten)]
    pub source: LlamacppCliStockToolsSource,
    pub flavor: LlamaCppFlavor,
}

impl LlamacppCliStockTools {
    /// The file a `GitHubRelease` downloads: its `asset` when set, else the
    /// flavor's upstream name at the release tag. `None` otherwise.
    pub fn release_asset(&self) -> Option<String> {
        let LlamacppCliStockToolsSource::GithubRelease(release) = &self.source else {
            return None;
        };
        match &release.asset {
            Some(asset) => Some(asset.to_string()),
            None => self
                .flavor
                .release_asset_name(release.repository_version.as_ref()),
        }
    }
}

/// The Android pipette app's in-process llama.cpp, always built from a git
/// checkout. No local HTTP server, so no `http_timeout_seconds` or raw `flags`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct LlamacppApkPipette {
    #[serde(flatten)]
    pub source: SourceRepository,
    pub flavor: LlamacppApkPipetteFlavor,
}

/// Build targets for the llama.cpp APK, one ABI today. Its own enum rather than
/// a bare marker, so a new ABI is an additive, compile-checked change.
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
pub enum LlamacppApkPipetteFlavor {
    AndroidArm64V8,
}

/// The iOS pipette app's in-process llama.cpp — `LlamacppApkPipette`'s
/// counterpart on iOS (same shape, same reasons).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct LlamacppIosPipette {
    #[serde(flatten)]
    pub source: SourceRepository,
    pub flavor: LlamacppIosPipetteFlavor,
    /// The private Apple APIs the build compiled in. Part of the runtime's identity, not a
    /// flag: they decide what the readiness gate sees and what `max_memory_usage` reads.
    #[serde(flatten)]
    pub private_apis: PrivateApis,
}

/// Build targets for the llama.cpp iOS app. One member today (a single
/// ABI); its own enum for the same reasons as `LlamacppApkPipetteFlavor`.
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
pub enum LlamacppIosPipetteFlavor {
    IosArm64,
}

impl std::fmt::Display for LlamacppCliStockToolsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlamacppCliStockToolsSource::GithubRelease(release) => write!(f, "{release}"),
            LlamacppCliStockToolsSource::RemoteArchive { url } => write!(f, "{url}"),
            LlamacppCliStockToolsSource::VaultRemoteArchive { url, .. } => write!(f, "{url}"),
            LlamacppCliStockToolsSource::RelativeDir { dir } => write!(f, "relative-dir:{dir}"),
            LlamacppCliStockToolsSource::AbsoluteDir { dir } => write!(f, "absolute-dir:{dir}"),
        }
    }
}

/// [`SourceRepository`]'s `repo@tag`, then `/<asset>` when one is set: two
/// assets of one release are two builds.
impl std::fmt::Display for LlamacppCliStockToolsRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.repository_url, self.repository_version)?;
        match &self.asset {
            Some(asset) => write!(f, "/{asset}"),
            None => Ok(()),
        }
    }
}

impl std::fmt::Display for LlamacppCliStockTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.flavor)
    }
}

impl std::fmt::Display for LlamacppApkPipette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.flavor)
    }
}

impl std::fmt::Display for LlamacppIosPipette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.flavor)
    }
}

/// llama.cpp build targets pipette tracks, as one kebab-case string. Any other
/// string parses as `Custom`, an operator's own build (a private fork or a
/// vendor build), which only hosts with an install of that exact name run.
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
pub enum LlamaCppFlavor {
    MacosX64,
    MacosArm64,
    MacosArm64Kleidiai,
    LinuxX64Openvino,
    LinuxX64Rocm,
    LinuxX64Cpu,
    LinuxX64SyclFp16,
    LinuxX64SyclFp32,
    LinuxX64Vulkan,
    /// No `x64` in the wire form: plans wrote this string while it was
    /// `Custom`, and their digests must not move.
    #[strum(serialize = "linux-cuda-12-8")]
    LinuxX64Cuda128,
    /// The wire name omits the CUDA minor version. Upstream renamed the asset
    /// from CUDA 13.3 to 13.4.
    #[strum(serialize = "linux-x64-cuda-13")]
    LinuxX64Cuda13,
    LinuxArm64Cpu,
    /// The wire name omits the CUDA minor version. Upstream renamed the asset
    /// from CUDA 13.3 to 13.4.
    #[strum(serialize = "linux-arm64-cuda-13")]
    LinuxArm64Cuda13,
    LinuxArm64Vulkan,
    /// Upstream's Linux arm64 Snapdragon build. Upstream first published it in
    /// `b11099`.
    LinuxArm64Snapdragon,
    LinuxS390xCpu,
    #[strum(serialize = "linux-openeuler-aarch64-ascend-310p")]
    LinuxOpenEulerAarch64Ascend310p,
    #[strum(serialize = "linux-openeuler-x86-ascend-310p")]
    LinuxOpenEulerX86Ascend310p,
    #[strum(serialize = "linux-openeuler-aarch64-ascend-910b-aclgraph")]
    LinuxOpenEulerAarch64Ascend910bAclgraph,
    #[strum(serialize = "linux-openeuler-x86-ascend-910b-aclgraph")]
    LinuxOpenEulerX86Ascend910bAclgraph,
    WindowsX64Cpu,
    WindowsArm64Cpu,
    WindowsArm64OpenclAdreno,
    #[strum(serialize = "windows-x64-cuda-12-4")]
    WindowsX64Cuda124,
    /// Upstream shipped the last CUDA 13.1 build in `b9360`. This flavor is
    /// separate from [`Self::WindowsX64Cuda13`], so a version bump never moves
    /// a host from CUDA 13.1 to a later minor version.
    #[strum(serialize = "windows-x64-cuda-13-1")]
    WindowsX64Cuda131,
    /// The wire name omits the CUDA minor version. Upstream renamed the asset
    /// from CUDA 13.3 to 13.4.
    #[strum(serialize = "windows-x64-cuda-13")]
    WindowsX64Cuda13,
    /// The wire name omits the CUDA minor version. Upstream ships the asset
    /// with CUDA 13.4.
    #[strum(serialize = "windows-arm64-cuda-13")]
    WindowsArm64Cuda13,
    /// The wire name omits the OpenVINO version. Upstream first shipped this
    /// build in `b9677` with OpenVINO 2026.2. Upstream renamed the asset with
    /// each later OpenVINO release.
    WindowsX64Openvino,
    WindowsX64Vulkan,
    WindowsArm64Vulkan,
    WindowsX64Sycl,
    WindowsX64Hip,
    /// Wire form is the Android NDK ABI name (`android-arm64-v8a`).
    #[strum(serialize = "android-arm64-v8a")]
    AndroidArm64Cpu,
    /// Upstream's Snapdragon package (CPU, Adreno and Hexagon), published from
    /// `b11099` on. Wire form is upstream's suffix, not the NDK spelling, so
    /// records that named it as a `Custom` build keep their string.
    AndroidArm64Snapdragon,
    /// Operator-supplied build outside the known set. Round-trips
    /// through serde as the raw string.
    #[strum(default, transparent)]
    Custom(String),
}

impl LlamaCppFlavor {
    /// Every known variant, in declaration order; excludes [`Self::Custom`].
    /// Lets a consumer offer the set: [`Self::parse`] turns a typo into `Custom`.
    pub fn known() -> impl Iterator<Item = Self> {
        use strum::IntoEnumIterator as _;
        Self::iter().filter(|flavor| !flavor.is_custom())
    }

    /// Whether this is an operator-supplied string outside the known set.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    /// Wire form. Known variants emit their canonical
    /// kebab name; `Custom(s)` emits `s` verbatim.
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// The upstream `ggml-org/llama.cpp` release asset name for this flavor at
    /// `version`, or `None` for [`Self::Custom`] and for a `b<N>` tag past the
    /// last release a flavor shipped in.
    ///
    /// Not derived from [`Self::as_str`]: upstream's names differ from pipette's
    /// (`android-arm64-v8a` ships as `android-arm64`).
    ///
    /// A `b<N>` tag takes the first name still in use at `N`; any other tag, a
    /// fork's, takes the newest. A name this table has not caught up with is
    /// spelled in the plan instead ([`LlamacppCliStockToolsRelease::asset`]).
    ///
    /// The extension assumes every Windows asset starts with `win-` (the `.zip`
    /// family), and every asset before `b7224` was a `.zip`. Lives here because
    /// the runtime store and the CLI both need it and neither may depend on the
    /// other.
    pub fn release_asset_name(&self, version: &str) -> Option<String> {
        // Earlier names with the last build that shipped each, oldest first,
        // then the current name, or `None` once upstream stopped building it.
        let (earlier, current): (&[(u32, &str)], Option<&str>) = match self {
            Self::MacosX64 => (&[], Some("macos-x64")),
            Self::MacosArm64 => (&[], Some("macos-arm64")),
            Self::MacosArm64Kleidiai => (&[(9360, "macos-arm64-kleidiai")], None),
            Self::LinuxX64Openvino => (
                &[
                    (9675, "ubuntu-openvino-2026.0-x64"),
                    (9816, "ubuntu-openvino-2026.2-x64"),
                    (10486, "ubuntu-openvino-2026.2.1-x64"),
                    (10670, "ubuntu-openvino-2026.3-x64"),
                    (11022, "ubuntu-openvino-2026.3.1-x64"),
                    (11372, "ubuntu-openvino-2026.4-x64"),
                ],
                Some("ubuntu-openvino-2026.4.1-x64"),
            ),
            Self::LinuxX64Rocm => (
                &[
                    (10355, "ubuntu-rocm-7.2-x64"),
                    (10766, "ubuntu-rocm-7.14-x64"),
                ],
                Some("ubuntu-rocm-10.0-x64"),
            ),
            Self::LinuxX64Cpu => (&[], Some("ubuntu-x64")),
            Self::LinuxX64SyclFp16 => (&[], Some("ubuntu-sycl-fp16-x64")),
            Self::LinuxX64SyclFp32 => (&[], Some("ubuntu-sycl-fp32-x64")),
            Self::LinuxX64Vulkan => (&[], Some("ubuntu-vulkan-x64")),
            Self::LinuxX64Cuda128 => (&[], Some("ubuntu-cuda-12.8-x64")),
            Self::LinuxX64Cuda13 => (
                &[(11070, "ubuntu-cuda-13.3-x64")],
                Some("ubuntu-cuda-13.4-x64"),
            ),
            Self::LinuxArm64Cpu => (&[], Some("ubuntu-arm64")),
            Self::LinuxArm64Cuda13 => (
                &[(11070, "ubuntu-cuda-13.3-arm64")],
                Some("ubuntu-cuda-13.4-arm64"),
            ),
            Self::LinuxArm64Vulkan => (&[], Some("ubuntu-vulkan-arm64")),
            Self::LinuxArm64Snapdragon => (&[], Some("linux-arm64-snapdragon")),
            Self::LinuxS390xCpu => (&[(6919, "ubuntu-s390x-z15")], Some("ubuntu-s390x")),
            Self::LinuxOpenEulerAarch64Ascend310p => (&[(9334, "310p-openEuler-aarch64")], None),
            Self::LinuxOpenEulerX86Ascend310p => (&[(9334, "310p-openEuler-x86")], None),
            Self::LinuxOpenEulerAarch64Ascend910bAclgraph => {
                (&[(9334, "910b-openEuler-aarch64-aclgraph")], None)
            }
            Self::LinuxOpenEulerX86Ascend910bAclgraph => {
                (&[(9334, "910b-openEuler-x86-aclgraph")], None)
            }
            Self::WindowsX64Cpu => (&[], Some("win-cpu-x64")),
            Self::WindowsArm64Cpu => (&[], Some("win-cpu-arm64")),
            Self::WindowsArm64OpenclAdreno => (
                &[(5309, "win-llvm-arm64-opencl-adreno")],
                Some("win-opencl-adreno-arm64"),
            ),
            Self::WindowsX64Cuda124 => (
                &[
                    (5309, "win-cuda-cu12.4-x64"),
                    (5317, "win--cu12.4-x64"),
                    (5449, "win-cuda12.4-x64"),
                ],
                Some("win-cuda-12.4-x64"),
            ),
            Self::WindowsX64Cuda131 => (&[(9360, "win-cuda-13.1-x64")], None),
            Self::WindowsX64Cuda13 => (&[(10976, "win-cuda-13.3-x64")], Some("win-cuda-13.4-x64")),
            Self::WindowsArm64Cuda13 => (&[], Some("win-cuda-13.4-arm64")),
            Self::WindowsX64Openvino => (
                &[
                    (9816, "win-openvino-2026.2-x64"),
                    (10486, "win-openvino-2026.2.1-x64"),
                    (10670, "win-openvino-2026.3-x64"),
                    (11022, "win-openvino-2026.3.1-x64"),
                    (11372, "win-openvino-2026.4-x64"),
                ],
                Some("win-openvino-2026.4.1-x64"),
            ),
            Self::WindowsX64Vulkan => (&[], Some("win-vulkan-x64")),
            Self::WindowsArm64Vulkan => (&[], Some("win-vulkan-arm64")),
            Self::WindowsX64Sycl => (&[], Some("win-sycl-x64")),
            // HIP is ROCm on Windows; upstream renamed it with the Linux build.
            Self::WindowsX64Hip => (
                &[(10355, "win-hip-radeon-x64"), (10766, "win-rocm-7.14-x64")],
                Some("win-rocm-10.0-x64"),
            ),
            Self::AndroidArm64Cpu => (&[], Some("android-arm64")),
            Self::AndroidArm64Snapdragon => (&[], Some("android-arm64-snapdragon")),
            Self::Custom(_) => return None,
        };
        let build = version
            .strip_prefix('b')
            .and_then(|n| n.parse::<u32>().ok());
        let suffix = match build {
            Some(build) => earlier
                .iter()
                .find(|(last, _)| build <= *last)
                .map(|(_, name)| *name)
                .or(current)?,
            None => current.or_else(|| earlier.last().map(|(_, name)| *name))?,
        };
        let ext = if suffix.starts_with("win-") || build.is_some_and(|build| build < 7224) {
            "zip"
        } else {
            "tar.gz"
        };
        Some(format!("llama-{version}-bin-{suffix}.{ext}"))
    }

    /// Case-insensitive on the known set; anything else is `Custom(s)`, case
    /// preserved.
    pub fn parse(s: &str) -> Self {
        s.parse().unwrap_or_else(|_| Self::Custom(s.to_owned()))
    }
}

impl Serialize for LlamaCppFlavor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LlamaCppFlavor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

/// llama.cpp flash-attention mode. `auto` lets llama.cpp decide per build/model;
/// `on`/`off` force it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, strum::EnumIter)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LlamacppFlashAttention {
    Auto,
    On,
    Off,
}

impl LlamacppFlashAttention {
    /// The wire word, shared by the argv renderers and the result record.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// How llama.cpp gets a model's weights into memory: upstream's `llama_load_mode`, which
/// supersedes the deprecated `--mmap` / `--mlock` / `--direct-io` booleans. One knob, as the
/// modes are exclusive. The wire form follows the C enum's names; the argv word comes from
/// [`as_cli_word`](Self::as_cli_word).
///
/// Under [`Read`](Self::Read) the weights occupy anonymous RAM, which keeps
/// `max_memory_usage` comparable across platforms and makes a model too large for the device
/// fail rather than quietly stream from storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlamacppLoadMode {
    /// `LLAMA_LOAD_MODE_NONE`: read the weights into anonymous memory. Not named `None`,
    /// which reads as "unset" beside an `Option`; the wire and argv word stays `none`.
    #[serde(rename = "none")]
    Read,
    /// `LLAMA_LOAD_MODE_MMAP`: memory-map the file. llama.cpp's own default.
    Mmap,
    /// `LLAMA_LOAD_MODE_MLOCK`: read in, then lock the pages against eviction.
    Mlock,
    /// `LLAMA_LOAD_MODE_MMAP_MLOCK`: map the file and lock the mapping.
    MmapMlock,
    /// `LLAMA_LOAD_MODE_DIRECT_IO`: bypass the page cache where the platform supports it.
    DirectIo,
}

impl LlamacppLoadMode {
    /// The `--load-mode` token: upstream's `llama_load_mode_name`, which is not always the
    /// wire word.
    pub fn as_cli_word(&self) -> &'static str {
        match self {
            Self::Read => "none",
            Self::Mmap => "mmap",
            Self::Mlock => "mlock",
            Self::MmapMlock => "mmap+mlock",
            Self::DirectIo => "dio",
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;
    use crate::runtime::{app_source, cli_release, words_match_serde};
    use crate::*;

    #[test]
    fn llamacpp_cli_from_archive_url_parses() -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(
            r#"type = "llamacpp_cli_stock_tools"
source = "remote_archive"
url = "https://example.com/llama.tar.gz"
flavor = "macos-arm64""#,
        )
        .context("archive runtime should parse")?;
        let Runtime::LlamacppCliStockTools(rt) = &runtime else {
            anyhow::bail!("expected LlamacppCliStockTools");
        };
        assert!(matches!(
            rt.source,
            LlamacppCliStockToolsSource::RemoteArchive { .. }
        ));
        assert_eq!(rt.source.origin_slug(), "remote-archive");
        // The scheme survives construction.
        assert_eq!(rt.source.reference(), "https://example.com/llama.tar.gz");

        // An older plan spelled this scheme-less; it still lands on the same runtime.
        let legacy: Runtime = toml::from_str(
            r#"type = "llamacpp_cli_stock_tools"
source = "remote_archive"
url = "example.com/llama.tar.gz"
flavor = "macos-arm64""#,
        )
        .context("scheme-less archive runtime should still parse")?;
        assert_eq!(legacy, runtime);
        Ok(())
    }

    /// Pins the asset assembly (prefix, version, extension) per variant at one
    /// tag; the renames are pinned below.
    #[rstest::rstest]
    #[case(LlamaCppFlavor::MacosX64, Some("llama-b9305-bin-macos-x64.tar.gz"))]
    #[case(LlamaCppFlavor::MacosArm64, Some("llama-b9305-bin-macos-arm64.tar.gz"))]
    #[case(
        LlamaCppFlavor::MacosArm64Kleidiai,
        Some("llama-b9305-bin-macos-arm64-kleidiai.tar.gz")
    )]
    // Diverges from the wire name `linux-x64-openvino`.
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        Some("llama-b9305-bin-ubuntu-openvino-2026.0-x64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Rocm,
        Some("llama-b9305-bin-ubuntu-rocm-7.2-x64.tar.gz")
    )]
    #[case(LlamaCppFlavor::LinuxX64Cpu, Some("llama-b9305-bin-ubuntu-x64.tar.gz"))]
    #[case(
        LlamaCppFlavor::LinuxX64SyclFp16,
        Some("llama-b9305-bin-ubuntu-sycl-fp16-x64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64SyclFp32,
        Some("llama-b9305-bin-ubuntu-sycl-fp32-x64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Vulkan,
        Some("llama-b9305-bin-ubuntu-vulkan-x64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Cuda128,
        Some("llama-b9305-bin-ubuntu-cuda-12.8-x64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Cuda13,
        Some("llama-b9305-bin-ubuntu-cuda-13.3-x64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxArm64Cpu,
        Some("llama-b9305-bin-ubuntu-arm64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxArm64Cuda13,
        Some("llama-b9305-bin-ubuntu-cuda-13.3-arm64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxArm64Vulkan,
        Some("llama-b9305-bin-ubuntu-vulkan-arm64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxArm64Snapdragon,
        Some("llama-b9305-bin-linux-arm64-snapdragon.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxS390xCpu,
        Some("llama-b9305-bin-ubuntu-s390x.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerAarch64Ascend310p,
        Some("llama-b9305-bin-310p-openEuler-aarch64.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerX86Ascend310p,
        Some("llama-b9305-bin-310p-openEuler-x86.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerAarch64Ascend910bAclgraph,
        Some("llama-b9305-bin-910b-openEuler-aarch64-aclgraph.tar.gz")
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerX86Ascend910bAclgraph,
        Some("llama-b9305-bin-910b-openEuler-x86-aclgraph.tar.gz")
    )]
    #[case(LlamaCppFlavor::WindowsX64Cpu, Some("llama-b9305-bin-win-cpu-x64.zip"))]
    #[case(
        LlamaCppFlavor::WindowsArm64Cpu,
        Some("llama-b9305-bin-win-cpu-arm64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsArm64OpenclAdreno,
        Some("llama-b9305-bin-win-opencl-adreno-arm64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Cuda124,
        Some("llama-b9305-bin-win-cuda-12.4-x64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Cuda131,
        Some("llama-b9305-bin-win-cuda-13.1-x64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Cuda13,
        Some("llama-b9305-bin-win-cuda-13.3-x64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsArm64Cuda13,
        Some("llama-b9305-bin-win-cuda-13.4-arm64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        Some("llama-b9305-bin-win-openvino-2026.2-x64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Vulkan,
        Some("llama-b9305-bin-win-vulkan-x64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsArm64Vulkan,
        Some("llama-b9305-bin-win-vulkan-arm64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Sycl,
        Some("llama-b9305-bin-win-sycl-x64.zip")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Hip,
        Some("llama-b9305-bin-win-hip-radeon-x64.zip")
    )]
    // Diverges from the wire name `android-arm64-v8a`.
    #[case(
        LlamaCppFlavor::AndroidArm64Cpu,
        Some("llama-b9305-bin-android-arm64.tar.gz")
    )]
    // Agrees with the wire name, unlike its CPU sibling above.
    #[case(
        LlamaCppFlavor::AndroidArm64Snapdragon,
        Some("llama-b9305-bin-android-arm64-snapdragon.tar.gz")
    )]
    // An operator-supplied build has no upstream asset.
    #[case(LlamaCppFlavor::Custom("weird".to_owned()), None)]
    fn llamacpp_flavor_release_asset_names(
        #[case] variant: LlamaCppFlavor,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(variant.release_asset_name("b9305").as_deref(), expected);
    }

    /// The catalog importer ignores the six `cudart-*` archives, `*-ui.tar.gz`,
    /// and the xcframework. Every other `b11669` asset is a distinct build flavor.
    #[test]
    fn every_b11669_build_asset_has_one_flavor() {
        use std::collections::BTreeSet;

        let expected: BTreeSet<_> = [
            "llama-b11669-bin-android-arm64-snapdragon.tar.gz",
            "llama-b11669-bin-android-arm64.tar.gz",
            "llama-b11669-bin-linux-arm64-snapdragon.tar.gz",
            "llama-b11669-bin-macos-arm64.tar.gz",
            "llama-b11669-bin-macos-x64.tar.gz",
            "llama-b11669-bin-ubuntu-arm64.tar.gz",
            "llama-b11669-bin-ubuntu-cuda-12.8-x64.tar.gz",
            "llama-b11669-bin-ubuntu-cuda-13.4-arm64.tar.gz",
            "llama-b11669-bin-ubuntu-cuda-13.4-x64.tar.gz",
            "llama-b11669-bin-ubuntu-openvino-2026.4.1-x64.tar.gz",
            "llama-b11669-bin-ubuntu-rocm-10.0-x64.tar.gz",
            "llama-b11669-bin-ubuntu-s390x.tar.gz",
            "llama-b11669-bin-ubuntu-sycl-fp16-x64.tar.gz",
            "llama-b11669-bin-ubuntu-sycl-fp32-x64.tar.gz",
            "llama-b11669-bin-ubuntu-vulkan-arm64.tar.gz",
            "llama-b11669-bin-ubuntu-vulkan-x64.tar.gz",
            "llama-b11669-bin-ubuntu-x64.tar.gz",
            "llama-b11669-bin-win-cpu-arm64.zip",
            "llama-b11669-bin-win-cpu-x64.zip",
            "llama-b11669-bin-win-cuda-12.4-x64.zip",
            "llama-b11669-bin-win-cuda-13.4-arm64.zip",
            "llama-b11669-bin-win-cuda-13.4-x64.zip",
            "llama-b11669-bin-win-opencl-adreno-arm64.zip",
            "llama-b11669-bin-win-openvino-2026.4.1-x64.zip",
            "llama-b11669-bin-win-rocm-10.0-x64.zip",
            "llama-b11669-bin-win-sycl-x64.zip",
            "llama-b11669-bin-win-vulkan-arm64.zip",
            "llama-b11669-bin-win-vulkan-x64.zip",
        ]
        .into_iter()
        .collect();
        assert_eq!(expected.len(), 28);
        let actual: Vec<_> = LlamaCppFlavor::known()
            .filter_map(|flavor| flavor.release_asset_name("b11669"))
            .collect();
        let unique: BTreeSet<_> = actual.iter().map(String::as_str).collect();
        assert_eq!(actual.len(), unique.len(), "two flavors named one asset");
        assert_eq!(unique, expected);
    }

    /// At each rename boundary, every known flavor names a different asset.
    #[rstest::rstest]
    #[case("b9360")]
    #[case("b10976")]
    #[case("b11070")]
    fn no_two_flavors_name_one_asset_at_a_rename(#[case] version: &str) {
        let actual: Vec<_> = LlamaCppFlavor::known()
            .filter_map(|flavor| flavor.release_asset_name(version))
            .collect();
        let unique: std::collections::BTreeSet<_> = actual.iter().collect();
        assert_eq!(actual.len(), unique.len(), "two flavors named one asset");
    }

    /// Each rename lands on the build upstream made it in, so the last build
    /// under a name and the first under the next are both pinned. A flavor
    /// upstream stopped building names nothing past its last release.
    #[rstest::rstest]
    #[case(LlamaCppFlavor::LinuxX64Rocm, "b10355", Some("ubuntu-rocm-7.2-x64"))]
    #[case(LlamaCppFlavor::LinuxX64Rocm, "b10356", Some("ubuntu-rocm-7.14-x64"))]
    #[case(LlamaCppFlavor::LinuxX64Rocm, "b10766", Some("ubuntu-rocm-7.14-x64"))]
    #[case(LlamaCppFlavor::LinuxX64Rocm, "b10767", Some("ubuntu-rocm-10.0-x64"))]
    #[case(LlamaCppFlavor::LinuxX64Rocm, "b11399", Some("ubuntu-rocm-10.0-x64"))]
    #[case(LlamaCppFlavor::WindowsX64Hip, "b10355", Some("win-hip-radeon-x64"))]
    #[case(LlamaCppFlavor::WindowsX64Hip, "b10356", Some("win-rocm-7.14-x64"))]
    #[case(LlamaCppFlavor::WindowsX64Hip, "b10767", Some("win-rocm-10.0-x64"))]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b9675",
        Some("ubuntu-openvino-2026.0-x64")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b9677",
        Some("ubuntu-openvino-2026.2-x64")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b9817",
        Some("ubuntu-openvino-2026.2.1-x64")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b10488",
        Some("ubuntu-openvino-2026.3-x64")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b10672",
        Some("ubuntu-openvino-2026.3.1-x64")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b11024",
        Some("ubuntu-openvino-2026.4-x64")
    )]
    #[case(
        LlamaCppFlavor::LinuxX64Openvino,
        "b11374",
        Some("ubuntu-openvino-2026.4.1-x64")
    )]
    #[case(LlamaCppFlavor::LinuxX64Cuda13, "b11070", Some("ubuntu-cuda-13.3-x64"))]
    #[case(LlamaCppFlavor::LinuxX64Cuda13, "b11071", Some("ubuntu-cuda-13.4-x64"))]
    #[case(
        LlamaCppFlavor::LinuxArm64Cuda13,
        "b11070",
        Some("ubuntu-cuda-13.3-arm64")
    )]
    #[case(
        LlamaCppFlavor::LinuxArm64Cuda13,
        "b11071",
        Some("ubuntu-cuda-13.4-arm64")
    )]
    #[case(LlamaCppFlavor::WindowsX64Cuda13, "b10976", Some("win-cuda-13.3-x64"))]
    #[case(LlamaCppFlavor::WindowsX64Cuda13, "b10977", Some("win-cuda-13.4-x64"))]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b9816",
        Some("win-openvino-2026.2-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b9817",
        Some("win-openvino-2026.2.1-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b10486",
        Some("win-openvino-2026.2.1-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b10488",
        Some("win-openvino-2026.3-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b10670",
        Some("win-openvino-2026.3-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b10672",
        Some("win-openvino-2026.3.1-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b11022",
        Some("win-openvino-2026.3.1-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b11024",
        Some("win-openvino-2026.4-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b11372",
        Some("win-openvino-2026.4-x64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Openvino,
        "b11374",
        Some("win-openvino-2026.4.1-x64")
    )]
    #[case(LlamaCppFlavor::LinuxS390xCpu, "b6919", Some("ubuntu-s390x-z15"))]
    #[case(LlamaCppFlavor::LinuxS390xCpu, "b6920", Some("ubuntu-s390x"))]
    // Every asset was a `.zip` until `b7224`, where the rest moved to `.tar.gz`.
    #[case(LlamaCppFlavor::LinuxX64Cpu, "b7223", Some("ubuntu-x64"))]
    #[case(LlamaCppFlavor::LinuxX64Cpu, "b7224", Some("ubuntu-x64"))]
    #[case(
        LlamaCppFlavor::WindowsArm64OpenclAdreno,
        "b5309",
        Some("win-llvm-arm64-opencl-adreno")
    )]
    #[case(
        LlamaCppFlavor::WindowsArm64OpenclAdreno,
        "b5310",
        Some("win-opencl-adreno-arm64")
    )]
    #[case(
        LlamaCppFlavor::WindowsX64Cuda124,
        "b5309",
        Some("win-cuda-cu12.4-x64")
    )]
    #[case(LlamaCppFlavor::WindowsX64Cuda124, "b5310", Some("win--cu12.4-x64"))]
    #[case(LlamaCppFlavor::WindowsX64Cuda124, "b5318", Some("win-cuda12.4-x64"))]
    #[case(LlamaCppFlavor::WindowsX64Cuda124, "b5450", Some("win-cuda-12.4-x64"))]
    #[case(
        LlamaCppFlavor::MacosArm64Kleidiai,
        "b9360",
        Some("macos-arm64-kleidiai")
    )]
    #[case(LlamaCppFlavor::MacosArm64Kleidiai, "b9361", None)]
    #[case(LlamaCppFlavor::WindowsX64Cuda131, "b9361", None)]
    #[case(LlamaCppFlavor::LinuxOpenEulerX86Ascend310p, "b9335", None)]
    #[case(LlamaCppFlavor::LinuxOpenEulerX86Ascend910bAclgraph, "b9335", None)]
    // Upstream first shipped it in `b10969`; earlier tags get the name anyway.
    #[case(
        LlamaCppFlavor::LinuxX64Cuda128,
        "b10216",
        Some("ubuntu-cuda-12.8-x64")
    )]
    #[case(LlamaCppFlavor::LinuxX64Cuda13, "b10216", Some("ubuntu-cuda-13.3-x64"))]
    // A tag outside upstream's `b<N>` scheme takes the newest name.
    #[case(LlamaCppFlavor::LinuxX64Rocm, "fork-1", Some("ubuntu-rocm-10.0-x64"))]
    #[case(
        LlamaCppFlavor::MacosArm64Kleidiai,
        "fork-1",
        Some("macos-arm64-kleidiai")
    )]
    fn llamacpp_flavor_release_asset_follows_renames(
        #[case] variant: LlamaCppFlavor,
        #[case] tag: &str,
        #[case] suffix: Option<&str>,
    ) {
        let zip = variant.as_str().starts_with("windows-")
            || tag
                .strip_prefix('b')
                .is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n < 7224));
        let ext = if zip { "zip" } else { "tar.gz" };
        assert_eq!(
            variant.release_asset_name(tag),
            suffix.map(|suffix| format!("llama-{tag}-bin-{suffix}.{ext}"))
        );
    }

    /// A declared asset wins over the flavor's name, and is the only way to
    /// reach a flavor past its last upstream build. Other sources name none.
    #[test]
    fn release_asset_prefers_the_declared_asset() -> anyhow::Result<()> {
        let mut runtime = LlamacppCliStockTools {
            source: LlamacppCliStockToolsSource::GithubRelease(cli_release("b11399")?),
            flavor: LlamaCppFlavor::WindowsX64Cuda131,
        };
        assert_eq!(runtime.release_asset(), None);

        let LlamacppCliStockToolsSource::GithubRelease(release) = &mut runtime.source else {
            anyhow::bail!("expected a release");
        };
        release.asset = Some(FileName::try_new(
            "llama-b11399-bin-win-cuda-13.4-x64.zip".to_owned(),
        )?);
        assert_eq!(
            runtime.release_asset().as_deref(),
            Some("llama-b11399-bin-win-cuda-13.4-x64.zip")
        );

        runtime.flavor = LlamaCppFlavor::LinuxX64Rocm;
        runtime.source = LlamacppCliStockToolsSource::RemoteArchive {
            url: RemoteArchiveUrl::try_new("https://ex.com/a.tar.gz".to_owned())?,
        };
        assert_eq!(runtime.release_asset(), None);
        Ok(())
    }

    /// `asset` round-trips, and a release without one emits no key, so every
    /// descriptor written before the field keeps its digest.
    #[test]
    fn release_asset_wire_form() -> anyhow::Result<()> {
        let bare = serde_json::json!({
            "type": "llamacpp_cli_stock_tools",
            "source": "github_release",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": "b11399",
            "flavor": "linux-x64-rocm",
        });
        let runtime: Runtime = serde_json::from_value(bare.clone())?;
        assert_eq!(serde_json::to_value(&runtime)?, bare);

        let mut named = bare.clone();
        named["asset"] = "llama-b11399-bin-ubuntu-rocm-10.0-x64.tar.gz".into();
        let runtime: Runtime = serde_json::from_value(named.clone())?;
        assert_eq!(serde_json::to_value(&runtime)?, named);
        assert_ne!(descriptor_digest(&named)?, descriptor_digest(&bare)?);
        Ok(())
    }

    #[rstest::rstest]
    #[case(LlamaCppFlavor::MacosX64, "macos-x64")]
    #[case(LlamaCppFlavor::MacosArm64, "macos-arm64")]
    #[case(LlamaCppFlavor::MacosArm64Kleidiai, "macos-arm64-kleidiai")]
    #[case(LlamaCppFlavor::LinuxX64Openvino, "linux-x64-openvino")]
    #[case(LlamaCppFlavor::LinuxX64Rocm, "linux-x64-rocm")]
    #[case(LlamaCppFlavor::LinuxX64Cpu, "linux-x64-cpu")]
    #[case(LlamaCppFlavor::LinuxX64SyclFp16, "linux-x64-sycl-fp16")]
    #[case(LlamaCppFlavor::LinuxX64SyclFp32, "linux-x64-sycl-fp32")]
    #[case(LlamaCppFlavor::LinuxX64Vulkan, "linux-x64-vulkan")]
    #[case(LlamaCppFlavor::LinuxX64Cuda128, "linux-cuda-12-8")]
    #[case(LlamaCppFlavor::LinuxX64Cuda13, "linux-x64-cuda-13")]
    #[case(LlamaCppFlavor::LinuxArm64Cpu, "linux-arm64-cpu")]
    #[case(LlamaCppFlavor::LinuxArm64Cuda13, "linux-arm64-cuda-13")]
    #[case(LlamaCppFlavor::LinuxArm64Vulkan, "linux-arm64-vulkan")]
    #[case(LlamaCppFlavor::LinuxArm64Snapdragon, "linux-arm64-snapdragon")]
    #[case(LlamaCppFlavor::LinuxS390xCpu, "linux-s390x-cpu")]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerAarch64Ascend310p,
        "linux-openeuler-aarch64-ascend-310p"
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerX86Ascend310p,
        "linux-openeuler-x86-ascend-310p"
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerAarch64Ascend910bAclgraph,
        "linux-openeuler-aarch64-ascend-910b-aclgraph"
    )]
    #[case(
        LlamaCppFlavor::LinuxOpenEulerX86Ascend910bAclgraph,
        "linux-openeuler-x86-ascend-910b-aclgraph"
    )]
    #[case(LlamaCppFlavor::WindowsX64Cpu, "windows-x64-cpu")]
    #[case(LlamaCppFlavor::WindowsArm64Cpu, "windows-arm64-cpu")]
    #[case(
        LlamaCppFlavor::WindowsArm64OpenclAdreno,
        "windows-arm64-opencl-adreno"
    )]
    #[case(LlamaCppFlavor::WindowsX64Cuda124, "windows-x64-cuda-12-4")]
    #[case(LlamaCppFlavor::WindowsX64Cuda131, "windows-x64-cuda-13-1")]
    #[case(LlamaCppFlavor::WindowsX64Cuda13, "windows-x64-cuda-13")]
    #[case(LlamaCppFlavor::WindowsArm64Cuda13, "windows-arm64-cuda-13")]
    #[case(LlamaCppFlavor::WindowsX64Openvino, "windows-x64-openvino")]
    #[case(LlamaCppFlavor::WindowsX64Vulkan, "windows-x64-vulkan")]
    #[case(LlamaCppFlavor::WindowsArm64Vulkan, "windows-arm64-vulkan")]
    #[case(LlamaCppFlavor::WindowsX64Sycl, "windows-x64-sycl")]
    #[case(LlamaCppFlavor::WindowsX64Hip, "windows-x64-hip")]
    #[case(LlamaCppFlavor::AndroidArm64Cpu, "android-arm64-v8a")]
    #[case(LlamaCppFlavor::AndroidArm64Snapdragon, "android-arm64-snapdragon")]
    fn llamacpp_flavor_wire_names(
        #[case] variant: LlamaCppFlavor,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        #[derive(Deserialize, Serialize)]
        struct Row {
            flavor: LlamaCppFlavor,
        }
        let parsed: Row = toml::from_str(&format!(r#"flavor = "{wire}""#))?;
        assert_eq!(parsed.flavor, variant);
        let emitted = toml::to_string(&Row { flavor: variant })?;
        assert!(emitted.contains(wire), "emit missing {wire:?}: {emitted:?}");
        Ok(())
    }

    /// Open, unlike the MLX, vLLM and SGLang flavors: a plan naming an
    /// operator-local build parses rather than being refused.
    #[test]
    fn llamacpp_flavor_custom_round_trips_as_bare_string() -> anyhow::Result<()> {
        #[derive(Deserialize, Serialize)]
        struct Row {
            flavor: LlamaCppFlavor,
        }
        let parsed: Row = toml::from_str(r#"flavor = "acme-cuda12""#)?;
        assert_eq!(
            parsed.flavor,
            LlamaCppFlavor::Custom("acme-cuda12".to_string()),
        );
        assert!(parsed.flavor.is_custom());
        let emitted = toml::to_string(&parsed)?;
        assert!(
            emitted.contains("acme-cuda12"),
            "Custom must emit bare string; got {emitted:?}",
        );

        // A known string parses as its variant: a `Custom` holding one is
        // ill-formed.
        let parsed: Row = toml::from_str(r#"flavor = "macos-arm64""#)?;
        assert_eq!(parsed.flavor, LlamaCppFlavor::MacosArm64);
        assert!(!parsed.flavor.is_custom());
        Ok(())
    }

    #[rstest::rstest]
    #[case::custom("Acme-CUDA12", LlamaCppFlavor::Custom("Acme-CUDA12".to_owned()))]
    #[case::known("MACOS-ARM64", LlamaCppFlavor::MacosArm64)]
    #[case::plans_spelling("Linux-CUDA-12-8", LlamaCppFlavor::LinuxX64Cuda128)]
    // No alias: the linux-x64-* spelling stays an operator's build.
    #[case::pattern_spelling(
        "linux-x64-cuda-12-8",
        LlamaCppFlavor::Custom("linux-x64-cuda-12-8".to_owned())
    )]
    fn llamacpp_flavor_parse(
        #[case] input: &str,
        #[case] expected: LlamaCppFlavor,
    ) -> anyhow::Result<()> {
        assert_eq!(LlamaCppFlavor::parse(input), expected);
        Ok(())
    }

    #[test]
    fn linux_cuda_keeps_the_wire_form_plans_wrote() -> anyhow::Result<()> {
        let authored = serde_json::json!({
            "type": "llamacpp_cli_stock_tools",
            "source": "remote_archive",
            "url": "https://example.com/llama-b10216-bin-linux-cuda-12.8-x64.tar.gz",
            "flavor": "linux-cuda-12-8",
        });
        let runtime: Runtime = serde_json::from_value(authored)?;
        let Runtime::LlamacppCliStockTools(tools) = &runtime else {
            anyhow::bail!("expected LlamacppCliStockTools");
        };
        assert_eq!(tools.flavor, LlamaCppFlavor::LinuxX64Cuda128);

        // What a build without the variant parsed the same plan into.
        let as_custom = Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: tools.source.clone(),
            flavor: LlamaCppFlavor::Custom("linux-cuda-12-8".to_owned()),
        });
        assert_eq!(descriptor_digest(&runtime)?, descriptor_digest(&as_custom)?);
        Ok(())
    }

    /// `known()` is the flavor set the CLI offers, so every member must name an
    /// upstream asset.
    #[test]
    fn every_known_flavor_is_offerable() -> anyhow::Result<()> {
        use strum::IntoEnumIterator as _;

        let known: Vec<_> = LlamaCppFlavor::known().collect();
        assert_eq!(
            known.len(),
            LlamaCppFlavor::iter().count() - 1,
            "known() must drop exactly `Custom`",
        );
        known.iter().for_each(|flavor| {
            assert_eq!(&LlamaCppFlavor::parse(flavor.as_str()), flavor);
            assert!(
                flavor.release_asset_name("b9305").is_some(),
                "{flavor} must name an upstream asset",
            );
        });
        Ok(())
    }

    #[test]
    fn runtime_source_relative_dir_round_trips_and_is_not_fetch_form() -> anyhow::Result<()> {
        let local = LlamacppCliStockToolsSource::RelativeDir {
            dir: RelativePath::try_new("blobs".to_owned())?,
        };
        assert!(matches!(
            local,
            LlamacppCliStockToolsSource::RelativeDir { .. }
        ));
        assert_eq!(local.reference(), "blobs");
        assert_eq!(local.origin_slug(), "local");
        assert_eq!(local.to_string(), "relative-dir:blobs");

        let json = serde_json::to_value(&local)?;
        assert_eq!(
            json,
            serde_json::json!({"source": "relative_dir", "dir": "blobs"})
        );
        let reparsed: LlamacppCliStockToolsSource = serde_json::from_value(json)?;
        assert_eq!(reparsed, local);

        // Plan form (GitHubRelease / RemoteArchive) is not an install dir.
        let release = LlamacppCliStockToolsSource::GithubRelease(cli_release("b1")?);
        assert!(!matches!(
            release,
            LlamacppCliStockToolsSource::RelativeDir { .. }
        ));
        Ok(())
    }

    #[test]
    fn runtime_source_absolute_dir_round_trips() -> anyhow::Result<()> {
        let abs = LlamacppCliStockToolsSource::AbsoluteDir {
            dir: AbsolutePath::try_new("/ws/runtimes/key/blobs".to_owned())?,
        };
        assert_eq!(abs.reference(), "/ws/runtimes/key/blobs");
        assert_eq!(abs.to_string(), "absolute-dir:/ws/runtimes/key/blobs");
        let json = serde_json::to_value(&abs)?;
        assert_eq!(
            json,
            serde_json::json!({"source": "absolute_dir", "dir": "/ws/runtimes/key/blobs"})
        );
        assert_eq!(
            serde_json::from_value::<LlamacppCliStockToolsSource>(json)?,
            abs
        );
        Ok(())
    }

    /// Tagged `source` + path-nature validation on `dir`.
    #[rstest::rstest]
    #[case::relative_ok(serde_json::json!({"source": "relative_dir", "dir": "blobs"}), true)]
    #[case::absolute_ok(serde_json::json!({"source": "absolute_dir", "dir": "/ws/r/blobs"}), true)]
    #[case::github_ok(serde_json::json!({"source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1"}), true)]
    #[case::archive_ok(serde_json::json!({"source": "remote_archive", "url": "https://ex.com/a.tgz"}), true)]
    #[case::archive_scheme_less_still_parses(serde_json::json!({"source": "remote_archive", "url": "ex.com/a.tgz"}), true)]
    #[case::archive_rejects_other_schemes(serde_json::json!({"source": "remote_archive", "url": "file:///tmp/a.tgz"}), false)]
    #[case::relative_rejects_abs(serde_json::json!({"source": "relative_dir", "dir": "/abs"}), false)]
    #[case::absolute_rejects_rel(serde_json::json!({"source": "absolute_dir", "dir": "blobs"}), false)]
    #[case::untagged_dir_rejected(serde_json::json!({"dir": "blobs"}), false)]
    #[case::missing_source_rejected(serde_json::json!({"repository_version": "b1"}), false)]
    fn runtime_source_path_wire(#[case] v: serde_json::Value, #[case] ok: bool) {
        assert_eq!(
            serde_json::from_value::<LlamacppCliStockToolsSource>(v).is_ok(),
            ok
        );
    }

    /// Each arm's `--runtime` ref token, submission `runtime_name`, checkpoint
    /// origin slug and identity; the runtime's identity appends the flavor.
    #[rstest::rstest]
    #[case::github_release(
        serde_json::json!({"source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b9050"}),
        "b9050",
        "github.com/ggml-org/llama.cpp",
        "ggml-org/llama.cpp",
        "github.com/ggml-org/llama.cpp@b9050"
    )]
    #[case::github_release_with_asset(
        serde_json::json!({"source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b9050", "asset": "llama.tgz"}),
        "b9050",
        "github.com/ggml-org/llama.cpp",
        "ggml-org/llama.cpp",
        "github.com/ggml-org/llama.cpp@b9050/llama.tgz"
    )]
    #[case::remote_archive(
        serde_json::json!({"source": "remote_archive", "url": "https://mirror.example/llama.tgz"}),
        "https://mirror.example/llama.tgz",
        "https://mirror.example/llama.tgz",
        "remote-archive",
        "https://mirror.example/llama.tgz"
    )]
    #[case::vault_remote_archive(
        serde_json::json!({
            "source": "vault_remote_archive",
            "url": "/vault/resources/vault_1/content",
            "principal_id": "org_1",
        }),
        "vault_1",
        "/vault/resources/vault_1/content",
        "vault-remote-archive",
        "/vault/resources/vault_1/content"
    )]
    #[case::relative_dir(
        serde_json::json!({"source": "relative_dir", "dir": "blobs"}),
        "blobs",
        "blobs",
        "local",
        "relative-dir:blobs"
    )]
    #[case::absolute_dir(
        serde_json::json!({"source": "absolute_dir", "dir": "/opt/llama"}),
        "/opt/llama",
        "/opt/llama",
        "local",
        "absolute-dir:/opt/llama"
    )]
    fn each_source_arm_names_itself(
        #[case] source: serde_json::Value,
        #[case] reference: &str,
        #[case] runtime_name: &str,
        #[case] origin_slug: &str,
        #[case] identity: &str,
    ) -> anyhow::Result<()> {
        let source: LlamacppCliStockToolsSource = serde_json::from_value(source)?;
        assert_eq!(source.reference(), reference);
        assert_eq!(source.runtime_name(), runtime_name);
        assert_eq!(source.origin_slug(), origin_slug);
        assert_eq!(source.to_string(), identity);
        let runtime = LlamacppCliStockTools {
            source,
            flavor: LlamaCppFlavor::MacosArm64,
        };
        assert_eq!(runtime.to_string(), format!("{identity}:macos-arm64"));
        Ok(())
    }

    // Pins each in-process app runtime's wire tag and flavor spelling.
    #[rstest::rstest]
    #[case::apk("llamacpp_apk_pipette", "android-arm64-v8")]
    #[case::ios("llamacpp_ios_pipette", "ios-arm64")]
    fn in_process_pipette_runtime_parses(
        #[case] type_tag: &str,
        #[case] flavor: &str,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(&format!(
            "type = \"{type_tag}\"\nrepository_url = \"github.com/ggml-org/llama.cpp\"\nversion = \"b5000\"\nflavor = \"{flavor}\""
        ))
        .context("in-process runtime should parse")?;
        assert_eq!(
            serde_json::to_value(&runtime)?,
            serde_json::json!({
                "type": type_tag,
                "repository_url": "github.com/ggml-org/llama.cpp",
                "repository_version": "b5000",
                "flavor": flavor,
            })
        );
        assert_eq!(
            runtime.to_string(),
            format!("github.com/ggml-org/llama.cpp@b5000:{flavor}")
        );
        // The app resolves its build by ref and flavor, so the repo stays out.
        assert_eq!(runtime.cli_ref(), format!("b5000:{flavor}"));
        Ok(())
    }

    /// Private APIs are part of the runtime's identity, so a device with the stock build
    /// refuses a plan that needs the gated one.
    #[test]
    fn private_thermal_separates_two_otherwise_identical_ios_runtimes() -> anyhow::Result<()> {
        let stock = LlamacppIosPipette {
            source: app_source("b10216")?,
            flavor: LlamacppIosPipetteFlavor::IosArm64,
            private_apis: PrivateApis::default(),
        };
        let gated = LlamacppIosPipette {
            private_apis: PrivateApis {
                thermal: true,
                memory: false,
            },
            ..stock.clone()
        };

        assert_ne!(stock, gated);
        // Absent means stock, so every plan written before this field keeps its meaning.
        let absent: LlamacppIosPipette = serde_json::from_str(
            r#"{"repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b10216","flavor":"ios-arm64"}"#,
        )?;
        assert_eq!(absent, stock);
        // And a stock runtime serializes without the field, so the wire form is unchanged.
        let wire = serde_json::to_value(&stock)?;
        assert!(wire.get("private_thermal").is_none(), "{wire}");
        assert_eq!(
            serde_json::to_value(&gated)?.get("private_thermal"),
            Some(&serde_json::Value::Bool(true))
        );
        Ok(())
    }

    /// The tools want other words for `mmap_mlock` and `direct_io`, so each mode pins both.
    #[rstest::rstest]
    #[case::read(LlamacppLoadMode::Read, "none", "none")]
    #[case::mmap(LlamacppLoadMode::Mmap, "mmap", "mmap")]
    #[case::mlock(LlamacppLoadMode::Mlock, "mlock", "mlock")]
    #[case::mmap_mlock(LlamacppLoadMode::MmapMlock, "mmap_mlock", "mmap+mlock")]
    #[case::direct_io(LlamacppLoadMode::DirectIo, "direct_io", "dio")]
    fn load_mode_spellings(
        #[case] mode: LlamacppLoadMode,
        #[case] wire: &str,
        #[case] cli_word: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(serde_json::to_value(mode)?, wire);
        assert_eq!(
            serde_json::from_value::<LlamacppLoadMode>(wire.into())?,
            mode
        );
        assert_eq!(mode.as_cli_word(), cli_word);
        // A plan is spelled in plan words, not argv words.
        assert_eq!(
            serde_json::from_value::<LlamacppLoadMode>(cli_word.into()).is_ok(),
            cli_word == wire
        );
        Ok(())
    }

    /// `as_str` reaches the argv, so it must be the word the plan wrote.
    #[test]
    fn argv_words_match_the_wire() -> anyhow::Result<()> {
        use strum::IntoEnumIterator as _;

        words_match_serde(LlamacppFlashAttention::as_str)?;
        assert_eq!(
            LlamacppFlashAttention::iter()
                .map(|mode| mode.as_str())
                .collect::<Vec<_>>(),
            ["auto", "on", "off"]
        );
        Ok(())
    }

    /// A vault archive is its own fetch, not `RemoteArchive` with a flag.
    #[test]
    fn vault_remote_archive_carries_its_principal() -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(
            r#"type = "llamacpp_cli_stock_tools"
source = "vault_remote_archive"
url = "/vault/resources/vault_1/content"
principal_id = "org_01936d"
flavor = "macos-arm64""#,
        )
        .context("vault archive runtime should parse")?;
        let Runtime::LlamacppCliStockTools(rt) = &runtime else {
            anyhow::bail!("expected LlamacppCliStockTools");
        };
        let LlamacppCliStockToolsSource::VaultRemoteArchive { principal_id, .. } = &rt.source
        else {
            anyhow::bail!("expected a vault archive");
        };
        assert_eq!(principal_id.to_string(), "org_01936d");
        assert_eq!(rt.source.origin_slug(), "vault-remote-archive");
        assert_eq!(rt.source.reference(), "vault_1");
        assert_eq!(rt.source.runtime_name(), "/vault/resources/vault_1/content");
        assert_eq!(rt.source.to_string(), "/vault/resources/vault_1/content");
        let back: Runtime = serde_json::from_str(&serde_json::to_string(&runtime)?)?;
        assert_eq!(runtime, back);

        // Its `custom_label` is kept and names nothing: the forms above stand.
        let mut labelled = serde_json::to_value(&runtime)?;
        labelled["custom_label"] = serde_json::json!("acme llama.cpp build");
        let labelled: Runtime = serde_json::from_value(labelled)?;
        assert_ne!(labelled, runtime);
        assert_eq!(labelled.to_string(), runtime.to_string());
        assert_eq!(
            serde_json::to_value(&labelled)?["custom_label"],
            "acme llama.cpp build"
        );
        Ok(())
    }
}
