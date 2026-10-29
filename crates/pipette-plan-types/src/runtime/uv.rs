//! The venv every uv-hosted runtime installs, and the version strings that name it.

use nutype::nutype;
use serde::{Deserialize, Serialize};

use crate::{
    AbsolutePath, NonEmptyString, NonEmptyTrimmedString, RelativePath, VaultPath, VaultPrincipalId,
};

/// How a uv-hosted runtime's environment is obtained. Plans use
/// [`Self::PipRequirementsText`] or [`Self::VaultWheelhouse`]; the preinstalled
/// arms are the effective form after a pull.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UvRuntimeSource {
    /// An environment defined by the exact `requirements.txt` body, which is its
    /// identity (the storage key digests it). Not a preset name: nothing would
    /// verify it, and a wrong one would alias two environments onto one entry.
    PipRequirementsText {
        contents: NonEmptyString,
        /// `uv pip install`/`compile` flags. Not identity: they alter how the
        /// body is installed, not which body it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        install_flags: Option<Vec<String>>,
    },
    /// The same body, installed with `--find-links` over a wheel archive mgmt
    /// serves to one principal. A wheelhouse, not a venv: a venv records
    /// absolute interpreter paths and cannot move between hosts.
    VaultWheelhouse {
        contents: NonEmptyString,
        url: VaultPath,
        principal_id: VaultPrincipalId,
        /// The vault record's `name`, which ingestion checks: a path says only an id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
        custom_label: Option<NonEmptyTrimmedString>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        install_flags: Option<Vec<String>>,
    },
    /// Venv already present under the store entry (entry-relative). Do not install.
    #[serde(alias = "preinstalled")]
    RelativePreinstalled { dir: RelativePath },
    /// Absolute venv root on the host after bind.
    #[serde(alias = "abs_preinstalled")]
    AbsolutePreinstalled { dir: AbsolutePath },
}

impl UvRuntimeSource {
    /// Install-time `uv` flags; empty for the preinstalled arms.
    pub fn install_flags(&self) -> &[String] {
        match self {
            Self::PipRequirementsText { install_flags, .. }
            | Self::VaultWheelhouse { install_flags, .. } => {
                install_flags.as_deref().unwrap_or(&[])
            }
            Self::RelativePreinstalled { .. } | Self::AbsolutePreinstalled { .. } => &[],
        }
    }

    /// Requirements body for the installable arms; `None` for preinstalled.
    pub fn requirements_text(&self) -> Option<&str> {
        match self {
            Self::PipRequirementsText { contents, .. } | Self::VaultWheelhouse { contents, .. } => {
                Some(contents.as_ref())
            }
            Self::RelativePreinstalled { .. } | Self::AbsolutePreinstalled { .. } => None,
        }
    }

    /// The principal this environment's wheels are served to, if it is a vault
    /// install. `None` for a public index or an already-present venv.
    pub fn principal_id(&self) -> Option<&VaultPrincipalId> {
        match self {
            Self::VaultWheelhouse { principal_id, .. } => Some(principal_id),
            Self::PipRequirementsText { .. }
            | Self::RelativePreinstalled { .. }
            | Self::AbsolutePreinstalled { .. } => None,
        }
    }
}

/// A version with optional build metadata after one `+`, such as
/// `0.10.0+cu121.py3.12`.
#[nutype(
    validate(regex = r"^[A-Za-z0-9._-]+(\+[A-Za-z0-9._-]+)?$"),
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
pub struct SemverWithBuild(String);

/// Server/package version for a uv-hosted OpenAI-compatible server.
/// This is the package version only, without build metadata.
#[nutype(
    validate(regex = r"^[A-Za-z0-9._-]+$"),
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
pub struct UvServerVersion(String);

/// Build selector for uv-hosted server wheels, such as `cu121` or `cpu`.
#[nutype(
    validate(regex = r"^(cpu|cu[A-Za-z0-9_-]*|rocm[A-Za-z0-9_-]*)$"),
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
pub struct UvBuild(String);

/// Python selector passed to `uv venv --python`. Path-free because it is
/// embedded in the derived runtime version.
#[nutype(
    validate(regex = r"^[0-9][A-Za-z0-9._-]*$"),
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
pub struct UvPythonVersion(String);

/// `<version>+<build>.py<python>`, the version every uv runtime that names a
/// build target renders.
pub fn uv_runtime_version(
    server_version: &UvServerVersion,
    build: &impl std::fmt::Display,
    python_version: &UvPythonVersion,
) -> String {
    format!("{server_version}+{build}.py{python_version}")
}

/// The venv source the uv-hosted runtimes' test fixtures install from.
#[cfg(test)]
pub(crate) fn sample_catalog_source() -> anyhow::Result<UvRuntimeSource> {
    Ok(UvRuntimeSource::PipRequirementsText {
        contents: NonEmptyString::try_new("vllm==0.10.0\n".to_owned())?,
        install_flags: None,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use super::*;
    use crate::*;

    #[test]
    fn uv_runtime_source_matrix() {
        let cases: &[(Option<&str>, bool)] = &[
            (
                Some(r#"type = "pip_requirements_text", contents = "vllm==0.10.0""#),
                true,
            ),
            (Some(r#"type = "preinstalled", dir = "blobs/venv""#), true),
            // A body is required, and `catalog_slug` is an unknown field.
            (Some(r#"type = "pip_requirements_text""#), false),
            (
                Some(r#"type = "pip_requirements_text", catalog_slug = "x""#),
                false,
            ),
            (Some(r#"type = "path", file = "/host/req.txt""#), false),
            (Some(r#"type = "catalog""#), false),
            (None, false),
            (Some(r#"type = "bogus""#), false),
        ];
        cases.iter().for_each(|(body, expect_ok)| {
            let runtime_toml = match body {
                None => r#"type = "uv_vllm"
server_version = "0.10.0"
build = "cu121"
python_version = "3.12""#
                    .to_owned(),
                Some(body) => format!(
                    r#"type = "uv_vllm"
server_version = "0.10.0"
build = "cu121"
python_version = "3.12"
source = {{ {body} }}"#
                ),
            };
            let parsed = toml::from_str::<Runtime>(&runtime_toml);
            assert_eq!(parsed.is_ok(), *expect_ok, "uv source {body:?}");
        });
    }

    #[test]
    fn uv_runtime_components_reject_malformed_fields() {
        [
            ("0.10.0", "xpu", "3.12"),
            ("0.10.0", "cu121.py3.12", "3.12"),
            ("0.10.0", "rocm6.py3.12", "3.12"),
            ("0.10.0+cu121", "cu121", "3.12"),
            ("0.10.0", "cu121", "py3.12"),
        ]
        .into_iter()
        .for_each(|(server_version, build, python_version)| {
            let raw = format!(
                r#"
                server_version = "{server_version}"
                build = "{build}"
                python_version = "{python_version}"
                source = {{ type = "pip_requirements_text", contents = "vllm==1" }}
                "#
            );
            assert!(
                toml::from_str::<UvVllm>(&raw).is_err(),
                "{server_version}+{build}.py{python_version} should reject"
            );
        });
    }

    #[test]
    fn uv_python_version_round_trips() -> anyhow::Result<()> {
        let rt = UvVllm {
            server_version: UvServerVersion::try_new("0.10.0".to_owned())?,
            build: UvBuild::try_new("cu121".to_owned())?,
            python_version: UvPythonVersion::try_new("3.11".to_owned())?,
            source: sample_catalog_source()?,
        };
        let emitted = toml::to_string(&rt)?;
        assert!(emitted.contains("python_version = \"3.11\""));
        assert!(!emitted.contains("\nversion = "));
        let parsed: UvVllm = toml::from_str(&emitted)?;
        assert_eq!(parsed, rt);
        Ok(())
    }

    #[test]
    fn uv_source_install_flags_optional_omitted_and_round_trip() -> anyhow::Result<()> {
        let bare = UvVllm {
            server_version: UvServerVersion::try_new("0.10.0".to_owned())?,
            build: UvBuild::try_new("cu121".to_owned())?,
            python_version: UvPythonVersion::try_new("3.12".to_owned())?,
            source: sample_catalog_source()?,
        };
        let bare_json = serde_json::to_value(&bare)?;
        assert!(
            bare_json["source"].get("install_flags").is_none(),
            "None install_flags must be omitted from the wire: {bare_json}"
        );

        let with_flags = UvVllm {
            source: UvRuntimeSource::PipRequirementsText {
                contents: NonEmptyString::try_new("vllm==0.10.0\n".to_owned())?,
                install_flags: Some(vec!["--index-strategy".into(), "unsafe-best-match".into()]),
            },
            ..bare
        };
        assert_eq!(
            with_flags.source.install_flags(),
            &[
                "--index-strategy".to_owned(),
                "unsafe-best-match".to_owned()
            ][..]
        );
        let flagged_json = serde_json::to_value(&with_flags)?;
        assert_eq!(
            flagged_json["source"].get("install_flags"),
            Some(&serde_json::json!([
                "--index-strategy",
                "unsafe-best-match"
            ]))
        );
        let reparsed: UvVllm = serde_json::from_value(flagged_json)?;
        assert_eq!(reparsed, with_flags);
        Ok(())
    }

    #[test]
    fn uv_runtime_source_catalog_pip_and_preinstalled_round_trip() -> anyhow::Result<()> {
        let catalog = sample_catalog_source()?;
        let catalog_json = serde_json::to_value(&catalog)?;
        assert_eq!(catalog_json["type"], "pip_requirements_text");
        assert!(catalog_json.get("catalog_slug").is_none());
        assert!(catalog_json.get("install_flags").is_none());
        assert_eq!(
            serde_json::from_value::<UvRuntimeSource>(catalog_json)?,
            catalog
        );

        let pip = UvRuntimeSource::PipRequirementsText {
            contents: NonEmptyString::try_new("vllm==0.10.0\n".to_owned())?,
            install_flags: None,
        };
        assert_eq!(pip.requirements_text(), Some("vllm==0.10.0\n"));
        let pip_json = serde_json::to_value(&pip)?;
        assert_eq!(pip_json["type"], "pip_requirements_text");
        assert_eq!(serde_json::from_value::<UvRuntimeSource>(pip_json)?, pip);

        let pre = UvRuntimeSource::RelativePreinstalled {
            dir: RelativePath::try_new("blobs/venv".to_owned())?,
        };
        assert!(pre.requirements_text().is_none());
        assert!(pre.install_flags().is_empty());
        assert!(pre.principal_id().is_none());
        let pre_json = serde_json::to_value(&pre)?;
        assert_eq!(pre_json["type"], "relative_preinstalled");
        assert_eq!(pre_json["dir"], "blobs/venv");
        assert_eq!(serde_json::from_value::<UvRuntimeSource>(pre_json)?, pre);
        // Legacy wire tag.
        let legacy = serde_json::json!({"type": "preinstalled", "dir": "blobs/venv"});
        assert_eq!(serde_json::from_value::<UvRuntimeSource>(legacy)?, pre);

        let mlx = MlxMacosPipette {
            version: NonEmptyString::try_new("0.31".to_owned())?,
            flavor: MlxMacosPipetteFlavor::MacosArm64,
            source: pre,
        };
        let mlx_json = serde_json::to_value(&mlx)?;
        assert_eq!(mlx_json["source"]["type"], "relative_preinstalled");
        assert_eq!(serde_json::from_value::<MlxMacosPipette>(mlx_json)?, mlx);
        Ok(())
    }

    /// The result must stay a valid `SemverWithBuild`.
    #[test]
    fn uv_runtime_version_joins_version_build_and_python() -> anyhow::Result<()> {
        let version = uv_runtime_version(
            &UvServerVersion::try_new("0.10.0")?,
            &UvBuild::try_new("rocm624")?,
            &UvPythonVersion::try_new("3.12")?,
        );
        assert_eq!(version, "0.10.0+rocm624.py3.12");
        assert!(SemverWithBuild::try_new(version).is_ok());
        Ok(())
    }

    #[test]
    fn uv_python_version_is_required() {
        let raw = r#"
            server_version = "0.10.0"
            build = "cu121"
            source = { type = "pip_requirements_text", contents = "vllm==1" }
        "#;
        assert!(toml::from_str::<UvVllm>(raw).is_err());
    }

    #[rstest]
    #[case::uv_vllm(
        "uv_vllm",
        "server_version = \"0.22.0\"\nbuild = \"cu129\"\npython_version = \"3.12\""
    )]
    #[case::uv_sglang(
        "uv_sglang",
        "server_version = \"0.5.12\"\nbuild = \"cu121\"\npython_version = \"3.12\""
    )]
    #[case::uv_openvino(
        "uv_openvino",
        "server_version = \"2026.2.1\"\npython_version = \"3.11\""
    )]
    #[case::uv_onnx_genai(
        "uv_onnx_genai",
        "server_version = \"0.16.0\"\nexecution_provider = \"cuda\"\npython_version = \"3.11\""
    )]
    #[case::mlx_macos_pipette(
        "mlx_macos_pipette",
        "version = \"0.20.0\"\nflavor = \"macos-arm64\""
    )]
    fn every_uv_runtime_can_install_from_a_vault_wheelhouse(
        #[case] runtime_type: &str,
        #[case] fields: &str,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = toml::from_str(&format!(
            "type = \"{runtime_type}\"\n{fields}\n\n[source]\ntype = \"vault_wheelhouse\"\ncontents = \"vllm==0.22.0\\n\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\""
        ))
        .context("a vault wheelhouse should parse for every uv runtime")?;
        let back: Runtime = serde_json::from_str(&serde_json::to_string(&runtime)?)
            .context("should round-trip through JSON")?;
        assert_eq!(runtime, back);
        Ok(())
    }

    #[test]
    fn a_wheelhouse_keeps_the_requirements_body_and_names_its_principal() -> anyhow::Result<()> {
        let public = UvRuntimeSource::PipRequirementsText {
            contents: NonEmptyString::try_new("vllm==0.22.0\n")?,
            install_flags: None,
        };
        let vault = UvRuntimeSource::VaultWheelhouse {
            contents: NonEmptyString::try_new("vllm==0.22.0\n")?,
            url: "/vault/resources/vault_1/content".parse()?,
            principal_id: "org_01936d".parse()?,
            custom_label: None,
            install_flags: Some(vec!["--no-index".to_owned()]),
        };
        assert_eq!(vault.requirements_text(), public.requirements_text());
        assert_eq!(vault.install_flags(), ["--no-index"]);
        assert!(public.principal_id().is_none());
        assert_eq!(
            vault.principal_id().map(ToString::to_string).as_deref(),
            Some("org_01936d")
        );

        let mut labelled = serde_json::to_value(&vault)?;
        labelled["custom_label"] = serde_json::json!("acme wheelhouse");
        let parsed: UvRuntimeSource = serde_json::from_value(labelled.clone())?;
        assert_eq!(serde_json::to_value(&parsed)?, labelled);
        Ok(())
    }

    #[rstest::rstest]
    #[case("0.10.0", true)] // bare semver
    #[case("0.31", true)] // two-part
    #[case("b9050", true)] // build label
    #[case("0.10.0+cu121.py3.12", true)] // build metadata
    #[case("0.10.0+rocm6", true)]
    #[case("0.10.0+cpu", true)]
    #[case("0.10.0+cu121-myfork", true)] // fork-distinguished build metadata
    #[case("a", true)] // single char
    #[case("", false)] // empty
    #[case("+cu121", false)] // empty pre-`+` segment
    #[case("0.10.0+", false)] // empty post-`+` segment
    #[case("0.10.0+a+b", false)] // multiple `+` separators
    #[case("weird/thing", false)] // path separator not in charset
    #[case(" 0.10.0", false)] // leading whitespace
    fn semver_with_build_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(SemverWithBuild::try_new(s).is_ok(), ok, "{s:?}");
    }
}
