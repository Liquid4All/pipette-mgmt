//! MLX: the macOS uv server and the in-process iOS app.

use serde::{Deserialize, Serialize};

use crate::{NonEmptyString, PrivateApis, SourceRepository, UvRuntimeSource};

/// The iOS pipette app's in-process MLX (Swift) runtime. Its version is the set
/// of pinned Swift packages, each a `SourceRepository`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct MlxIosPipette {
    pub packages: MlxSwiftStack,
    pub flavor: MlxIosPipetteFlavor,
    /// As [`LlamacppIosPipette::private_apis`](crate::LlamacppIosPipette::private_apis): they
    /// belong to the app, not the engine.
    #[serde(flatten)]
    pub private_apis: PrivateApis,
}

/// The pinned Swift-package stack the iOS MLX build was compiled against
/// (from the app's `Package.resolved`). Each is a repo + ref.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct MlxSwiftStack {
    pub mlx_swift: SourceRepository,
    pub mlx_swift_lm: SourceRepository,
    pub swift_transformers: SourceRepository,
}

/// Build targets for the MLX iOS app. One member today (a single ABI); its
/// own enum for the same reasons as `LlamacppIosPipetteFlavor`.
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
pub enum MlxIosPipetteFlavor {
    IosArm64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct MlxMacosPipette {
    pub version: NonEmptyString,
    pub flavor: MlxMacosPipetteFlavor,
    pub source: UvRuntimeSource,
}

impl std::fmt::Display for MlxMacosPipette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.version, f)
    }
}

impl std::fmt::Display for MlxIosPipette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.packages, self.flavor)
    }
}

/// The pinned Swift packages joined space-separated, each as its
/// `SourceRepository` coordinate (`repo@version`), in field order.
impl std::fmt::Display for MlxSwiftStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mlx-swift={} mlx-swift-lm={} swift-transformers={}",
            self.mlx_swift, self.mlx_swift_lm, self.swift_transformers
        )
    }
}

/// Closed set of MLX build targets pipette tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "clap", clap(rename_all = "kebab-case"))]
pub enum MlxMacosPipetteFlavor {
    MacosArm64,
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;
    use crate::*;

    #[rstest::rstest]
    #[case::macos_arm64("macos-arm64", Some(MlxMacosPipetteFlavor::MacosArm64))]
    #[case::unknown_apple_target("acme-arm64", None)]
    fn mlx_flavor_wire_names(
        #[case] wire: &str,
        #[case] expected: Option<MlxMacosPipetteFlavor>,
    ) -> anyhow::Result<()> {
        #[derive(Deserialize, Serialize)]
        struct Row {
            flavor: MlxMacosPipetteFlavor,
        }
        let parsed = toml::from_str::<Row>(&format!(r#"flavor = "{wire}""#)).ok();
        assert_eq!(parsed.map(|row| row.flavor), expected);
        if let Some(flavor) = expected {
            let emitted = toml::to_string(&Row { flavor })?;
            assert!(emitted.contains(wire), "emit missing {wire:?}: {emitted:?}");
        }
        Ok(())
    }

    /// The bare version is both the canonical id and the `--runtime` ref.
    #[test]
    fn mlx_macos_pipette_wire_and_display() -> anyhow::Result<()> {
        let wire = serde_json::json!({
            "type": "mlx_macos_pipette",
            "version": "0.20.0",
            "flavor": "macos-arm64",
            "source": { "type": "pip_requirements_text", "contents": "mlx-lm==0.20.0\n" },
        });
        let runtime: Runtime = serde_json::from_value(wire.clone())?;
        let Runtime::MlxMacosPipette(rt) = &runtime else {
            anyhow::bail!("expected MlxMacosPipette, got {runtime:?}");
        };
        assert_eq!(rt.to_string(), "0.20.0");
        assert_eq!(runtime.cli_ref(), "0.20.0");
        assert_eq!(serde_json::to_value(&runtime)?, wire);
        Ok(())
    }

    #[test]
    fn mlx_ios_flavor_parses_as_known_variant() -> anyhow::Result<()> {
        // `ios-arm64` is the on-device MLX flavor, not the desktop one's.
        #[derive(Deserialize)]
        struct Row {
            flavor: MlxIosPipetteFlavor,
        }
        let parsed: Row = toml::from_str(r#"flavor = "ios-arm64""#)?;
        assert_eq!(parsed.flavor, MlxIosPipetteFlavor::IosArm64);
        Ok(())
    }

    #[test]
    fn mlx_ios_pipette_runtime_parses() -> anyhow::Result<()> {
        // The on-device MLX runtime: three pinned Swift packages (each a
        // repo + ref) plus the iOS flavor.
        let runtime: Runtime = toml::from_str(
            r#"type = "mlx_ios_pipette"
flavor = "ios-arm64"

[packages]
mlx_swift = { repository_url = "github.com/ml-explore/mlx-swift", repository_version = "0.25.6" }
mlx_swift_lm = { repository_url = "github.com/ml-explore/mlx-swift-examples", repository_version = "2.25.6" }
swift_transformers = { repository_url = "github.com/huggingface/swift-transformers", repository_version = "0.1.22" }"#,
        )
        .context("mlx ios pipette runtime should parse")?;
        let Runtime::MlxIosPipette(rt) = &runtime else {
            anyhow::bail!("expected MlxIosPipette, got {runtime:?}");
        };
        assert_eq!(rt.flavor, MlxIosPipetteFlavor::IosArm64);
        assert_eq!(rt.packages.mlx_swift.repository_version.as_ref(), "0.25.6");
        assert_eq!(
            rt.packages.to_string(),
            "mlx-swift=github.com/ml-explore/mlx-swift@0.25.6 mlx-swift-lm=github.com/ml-explore/mlx-swift-examples@2.25.6 swift-transformers=github.com/huggingface/swift-transformers@0.1.22"
        );
        assert_eq!(rt.to_string(), format!("{}:ios-arm64", rt.packages));
        Ok(())
    }
}
