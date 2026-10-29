//! The gguf model kinds: a single text file, weights plus a vision projector,
//! and that pair with a drafter, each under every source it can be fetched from.

use serde::{Deserialize, Serialize};

use crate::{
    AbsolutePath, AuthToken, HfRepo, NonEmptyTrimmedString, RelativePath, RepoSubpath, ResourceUrl,
    Sha256, VaultPath, VaultPrincipalId,
};

// gguf text (single file)

/// Where a single-file gguf text model's weights live, by `source`: an HF repo
/// file, a local path, a direct `http(s)` URL, or a `vault_url` mgmt serves to
/// the principal's devices. The fetching arms take an optional `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum GgufTextSource {
    #[serde(rename = "huggingface")]
    HuggingFace {
        #[serde(flatten)]
        repo: HfRepo,
        path: RepoSubpath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<Sha256>,
    },
    /// Portable relative path (store layout / entry-relative).
    RelativeFile { path: RelativePath },
    /// Absolute host path after bind.
    AbsoluteFile { path: AbsolutePath },
    Url {
        url: ResourceUrl,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<Sha256>,
    },
    /// Its own arm, not a flag on `url`: it goes to mgmt signed as the device,
    /// and a caller matches the tag rather than remembering a field.
    VaultUrl {
        url: VaultPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<Sha256>,
        principal_id: VaultPrincipalId,
        /// The vault record's `name`, which ingestion checks: a path says only an id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
        custom_label: Option<NonEmptyTrimmedString>,
    },
}

impl GgufTextSource {
    /// Reference form: `<org>/<repo>[@<revision>]:<path>` for an HF file, else
    /// the local path or the URL. Routes through [`HfRepo::reference`] so a
    /// pinned revision is part of the identity.
    pub fn reference(&self) -> String {
        match self {
            GgufTextSource::HuggingFace { repo, path, .. } => {
                format!("{}:{path}", repo.reference())
            }
            GgufTextSource::RelativeFile { path } => path.to_string(),
            GgufTextSource::AbsoluteFile { path } => path.to_string(),
            GgufTextSource::Url { url, .. } => url.to_string(),
            GgufTextSource::VaultUrl { url, .. } => url.to_string(),
        }
    }

    /// The access token for this source, if any (only the HF arm can carry one).
    pub fn auth_token(&self) -> Option<&AuthToken> {
        match self {
            GgufTextSource::HuggingFace { repo, .. } => repo.auth_token.as_ref(),
            GgufTextSource::RelativeFile { .. }
            | GgufTextSource::AbsoluteFile { .. }
            | GgufTextSource::Url { .. }
            | GgufTextSource::VaultUrl { .. } => None,
        }
    }
}

/// Single-file gguf model (llama.cpp).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufText {
    #[serde(flatten)]
    pub source: GgufTextSource,
}

// gguf vision (weights + projector)

/// Where a VL gguf model's two files (main weights + projector) live, by
/// `source`. The fetching arms carry an optional per-file `_sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum GgufVisionSource {
    #[serde(rename = "huggingface")]
    HuggingFace(GgufVisionHfFiles),
    RelativeFiles(GgufVisionRelativeFiles),
    AbsoluteFiles(GgufVisionAbsoluteFiles),
    Url(GgufVisionUrlFiles),
    VaultUrl(GgufVisionVaultUrlFiles),
}

/// Both vision files in one HF repo, named by repo-relative path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVisionHfFiles {
    #[serde(flatten)]
    pub repo: HfRepo,
    pub model: RepoSubpath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_sha256: Option<Sha256>,
    pub mmproj: RepoSubpath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_sha256: Option<Sha256>,
}

/// Both vision files as portable relative paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVisionRelativeFiles {
    pub model: RelativePath,
    pub mmproj: RelativePath,
}

/// Both vision files as absolute host paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVisionAbsoluteFiles {
    pub model: AbsolutePath,
    pub mmproj: AbsolutePath,
}

/// Both vision files as direct `http(s)` downloads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVisionUrlFiles {
    pub model: ResourceUrl,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_sha256: Option<Sha256>,
    pub mmproj: ResourceUrl,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_sha256: Option<Sha256>,
}

/// Both vision files as vault resources served to one principal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVisionVaultUrlFiles {
    pub model: VaultPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_sha256: Option<Sha256>,
    pub mmproj: VaultPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_sha256: Option<Sha256>,
    pub principal_id: VaultPrincipalId,
    /// The `name` its files' vault records share, which ingestion checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub custom_label: Option<NonEmptyTrimmedString>,
}

impl GgufVisionSource {
    /// The access token for this source, if any (only the HF arm can carry one).
    pub fn auth_token(&self) -> Option<&AuthToken> {
        match self {
            GgufVisionSource::HuggingFace(f) => f.repo.auth_token.as_ref(),
            GgufVisionSource::RelativeFiles(_)
            | GgufVisionSource::AbsoluteFiles(_)
            | GgufVisionSource::Url(_)
            | GgufVisionSource::VaultUrl(_) => None,
        }
    }
}

/// VL gguf: main weights + a projector file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVision {
    #[serde(flatten)]
    pub source: GgufVisionSource,
}

// gguf vision + drafter (weights + projector + optional separate draft model)

/// Where a speculative VL gguf model's files live: a [`GgufVisionSource`]'s
/// files plus the drafter. `draft` holds the drafter's weights alone
/// (llama.cpp's `--model-draft`); absent means it is merged into `model`, and
/// the baseline arm loads that same file with speculation off.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum GgufVisionSpeculativeSource {
    #[serde(rename = "huggingface")]
    HuggingFace {
        #[serde(flatten)]
        files: GgufVisionHfFiles,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<RepoSubpath>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft_sha256: Option<Sha256>,
    },
    /// Portable relative paths.
    RelativeFiles {
        #[serde(flatten)]
        files: GgufVisionRelativeFiles,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<RelativePath>,
    },
    /// Absolute host paths.
    AbsoluteFiles {
        #[serde(flatten)]
        files: GgufVisionAbsoluteFiles,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<AbsolutePath>,
    },
    Url {
        #[serde(flatten)]
        files: GgufVisionUrlFiles,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<ResourceUrl>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft_sha256: Option<Sha256>,
    },
    VaultUrl {
        #[serde(flatten)]
        files: GgufVisionVaultUrlFiles,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<VaultPath>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft_sha256: Option<Sha256>,
    },
}

impl GgufVisionSpeculativeSource {
    /// The weights and projector as a plain [`GgufVisionSource`], so identity,
    /// storage key and download go through [`GgufVision`]'s.
    pub fn vision(&self) -> GgufVisionSource {
        match self {
            Self::HuggingFace { files, .. } => GgufVisionSource::HuggingFace(files.clone()),
            Self::RelativeFiles { files, .. } => GgufVisionSource::RelativeFiles(files.clone()),
            Self::AbsoluteFiles { files, .. } => GgufVisionSource::AbsoluteFiles(files.clone()),
            Self::Url { files, .. } => GgufVisionSource::Url(files.clone()),
            Self::VaultUrl { files, .. } => GgufVisionSource::VaultUrl(files.clone()),
        }
    }

    /// The separate drafter's identity, resolved against the same source as the
    /// weights; `None` when the drafter is merged into them.
    pub fn draft_reference(&self) -> Option<String> {
        match self {
            Self::HuggingFace { files, draft, .. } => draft
                .as_ref()
                .map(|d| format!("{}:{d}", files.repo.reference())),
            Self::RelativeFiles { draft, .. } => draft.as_ref().map(ToString::to_string),
            Self::AbsoluteFiles { draft, .. } => draft.as_ref().map(ToString::to_string),
            Self::Url { draft, .. } => draft.as_ref().map(ToString::to_string),
            Self::VaultUrl { draft, .. } => draft.as_ref().map(ToString::to_string),
        }
    }

    /// The access token for this source, if any (only the HF arm can carry one).
    pub fn auth_token(&self) -> Option<&AuthToken> {
        match self {
            Self::HuggingFace { files, .. } => files.repo.auth_token.as_ref(),
            Self::RelativeFiles { .. }
            | Self::AbsoluteFiles { .. }
            | Self::Url { .. }
            | Self::VaultUrl { .. } => None,
        }
    }
}

/// Speculative VL gguf: target weights + projector + drafter, merged or a
/// separate file. Its own type, not a flag on [`GgufVision`]: the drafter is
/// part of what was measured, so rows differing by it must not collide.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct GgufVisionSpeculative {
    #[serde(flatten)]
    pub source: GgufVisionSpeculativeSource,
}

impl std::fmt::Display for GgufText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.reference())
    }
}

impl std::fmt::Display for GgufVision {
    /// One identity for both files: `{weights}+{projector}`, each
    /// `<repo-ref>:<path>` for HF, else the local path or URL.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            GgufVisionSource::HuggingFace(s) => {
                let repo = s.repo.reference();
                write!(f, "{repo}:{}+{repo}:{}", s.model, s.mmproj)
            }
            GgufVisionSource::RelativeFiles(s) => write!(f, "{}+{}", s.model, s.mmproj),
            GgufVisionSource::AbsoluteFiles(s) => write!(f, "{}+{}", s.model, s.mmproj),
            GgufVisionSource::Url(s) => write!(f, "{}+{}", s.model, s.mmproj),
            GgufVisionSource::VaultUrl(s) => write!(f, "{}+{}", s.model, s.mmproj),
        }
    }
}

impl std::fmt::Display for GgufVisionSpeculative {
    /// `{weights}+{projector}[+{drafter}]`, extending [`GgufVision`]'s
    /// spelling; a merged drafter has no file, so it adds no segment.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        GgufVision {
            source: self.source.vision(),
        }
        .fmt(f)?;
        match self.source.draft_reference() {
            Some(draft) => write!(f, "+{draft}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;
    use crate::*;

    #[test]
    fn gguf_text_from_hf_parses_and_references() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"gguf_text\"\nsource = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\npath = \"Q4_K_M.gguf\"",
        )
        .context("hf gguf_text should parse")?;
        let Model::GgufText(m) = &model else {
            anyhow::bail!("expected GgufText");
        };
        assert!(matches!(m.source, GgufTextSource::HuggingFace { .. }));
        assert_eq!(m.source.reference(), "org/repo:Q4_K_M.gguf");
        assert_eq!(model.to_string(), "org/repo:Q4_K_M.gguf");
        Ok(())
    }

    #[test]
    fn gguf_text_from_local_file_parses_needs_no_auth() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"gguf_text\"\nsource = \"absolute_file\"\npath = \"/models/m.gguf\"",
        )
        .context("abs_local gguf_text should parse")?;
        let Model::GgufText(m) = &model else {
            anyhow::bail!("expected GgufText");
        };
        assert!(matches!(m.source, GgufTextSource::AbsoluteFile { .. }));
        assert_eq!(m.source.reference(), "/models/m.gguf");
        assert!(m.source.auth_token().is_none());
        assert!(model.auth_token().is_none());
        assert_eq!(model.to_string(), "/models/m.gguf");
        Ok(())
    }

    #[test]
    fn gguf_text_from_url_parses_with_optional_sha256() -> anyhow::Result<()> {
        let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let model: Model = toml::from_str(&format!(
            "type = \"gguf_text\"\nsource = \"url\"\nurl = \"https://cdn.example/m.gguf\"\nsha256 = \"{sha}\""
        ))
        .context("url gguf_text should parse")?;
        let Model::GgufText(m) = &model else {
            anyhow::bail!("expected GgufText");
        };
        assert!(matches!(m.source, GgufTextSource::Url { .. }));
        assert_eq!(model.to_string(), "https://cdn.example/m.gguf");
        assert!(model.auth_token().is_none());
        Ok(())
    }

    #[test]
    fn gguf_vision_from_hf_references_model_and_mmproj() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"gguf_vision\"\nsource = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\nmodel = \"a.gguf\"\nmmproj = \"mmproj.gguf\"",
        )
        .context("hf gguf_vision should parse")?;
        let Model::GgufVision(m) = &model else {
            anyhow::bail!("expected GgufVision");
        };
        assert!(matches!(m.source, GgufVisionSource::HuggingFace(_)));
        assert_eq!(model.to_string(), "org/repo:a.gguf+org/repo:mmproj.gguf");
        Ok(())
    }

    /// The arms that address each file on its own: the wire keys, the identity
    /// (both files joined by `+`), and no HF coordinate to carry a token.
    #[rstest::rstest]
    #[case::relative_files(
        "source = \"relative_files\"\nmodel = \"m/a.gguf\"\nmmproj = \"m/mmproj.gguf\"",
        serde_json::json!({"type": "gguf_vision", "source": "relative_files", "model": "m/a.gguf", "mmproj": "m/mmproj.gguf"}),
        "m/a.gguf+m/mmproj.gguf"
    )]
    #[case::absolute_files(
        "source = \"absolute_files\"\nmodel = \"/m/a.gguf\"\nmmproj = \"/m/mmproj.gguf\"",
        serde_json::json!({"type": "gguf_vision", "source": "absolute_files", "model": "/m/a.gguf", "mmproj": "/m/mmproj.gguf"}),
        "/m/a.gguf+/m/mmproj.gguf"
    )]
    // Only the pinned file's hash is on the wire.
    #[case::url(
        "source = \"url\"\nmodel = \"https://cdn.example/m.gguf\"\nmmproj = \"https://cdn.example/mmproj.gguf\"\nmmproj_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"",
        serde_json::json!({"type": "gguf_vision", "source": "url", "model": "https://cdn.example/m.gguf", "mmproj": "https://cdn.example/mmproj.gguf", "mmproj_sha256": "0000000000000000000000000000000000000000000000000000000000000000"}),
        "https://cdn.example/m.gguf+https://cdn.example/mmproj.gguf"
    )]
    fn gguf_vision_file_arms_name_both_files(
        #[case] source: &str,
        #[case] wire: serde_json::Value,
        #[case] identity: &str,
    ) -> anyhow::Result<()> {
        let model: Model = toml::from_str(&format!("type = \"gguf_vision\"\n{source}"))?;
        assert_eq!(serde_json::to_value(&model)?, wire);
        assert_eq!(model.to_string(), identity);
        assert!(model.auth_token().is_none());
        Ok(())
    }

    /// Each arm's files are typed by the arm, so a path of the wrong kind is
    /// refused at parse rather than resolved against the wrong root.
    #[rstest::rstest]
    #[case::absolute_in_relative_files(
        "source = \"relative_files\"\nmodel = \"/m/a.gguf\"\nmmproj = \"m/mmproj.gguf\"",
        "RelativePath"
    )]
    #[case::relative_in_absolute_files(
        "source = \"absolute_files\"\nmodel = \"/m/a.gguf\"\nmmproj = \"m/mmproj.gguf\"",
        "AbsolutePath"
    )]
    #[case::path_in_url(
        "source = \"url\"\nmodel = \"https://cdn.example/m.gguf\"\nmmproj = \"/m/mmproj.gguf\"",
        "ResourceUrl"
    )]
    fn gguf_vision_file_arms_refuse_a_path_of_another_kind(
        #[case] source: &str,
        #[case] refused_by: &str,
    ) -> anyhow::Result<()> {
        let err = toml::from_str::<Model>(&format!("type = \"gguf_vision\"\n{source}"))
            .err()
            .context("a path of another kind should be refused")?;
        assert!(err.to_string().contains(refused_by), "{err}");
        Ok(())
    }

    // Each arm is a newtype over a flattened struct inside an internally tagged
    // enum; pin that the wire keys survive both formats.
    #[rstest::rstest]
    #[case::huggingface(
        "source = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\nrevision = \"abc\"\nmodel = \"a.gguf\"\nmodel_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\nmmproj = \"mmproj.gguf\""
    )]
    #[case::url(
        "source = \"url\"\nmodel = \"https://cdn.example/m.gguf\"\nmmproj = \"https://cdn.example/mmproj.gguf\"\nmmproj_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\""
    )]
    #[case::relative_files(
        "source = \"relative_files\"\nmodel = \"m/a.gguf\"\nmmproj = \"m/mmproj.gguf\""
    )]
    #[case::absolute_files(
        "source = \"absolute_files\"\nmodel = \"/m/a.gguf\"\nmmproj = \"/m/mmproj.gguf\""
    )]
    fn gguf_vision_source_round_trips(#[case] body: &str) -> anyhow::Result<()> {
        let model: Model = toml::from_str(&format!("type = \"gguf_vision\"\n{body}"))?;
        assert_eq!(model, toml::from_str::<Model>(&toml::to_string(&model)?)?);
        assert_eq!(
            model,
            serde_json::from_str::<Model>(&serde_json::to_string(&model)?)?
        );
        Ok(())
    }

    /// Each arm's `vision()` is its `gguf_vision` source and the drafter
    /// resolves against it; a merged drafter has no reference and no segment.
    #[rstest::rstest]
    #[case::huggingface(
        "source = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\nmodel = \"a.gguf\"\nmmproj = \"mmproj.gguf\"",
        Some("draft.gguf"),
        Some("org/repo:draft.gguf"),
        "org/repo:a.gguf+org/repo:mmproj.gguf+org/repo:draft.gguf"
    )]
    #[case::huggingface_pinned(
        "source = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\nrevision = \"abc\"\nmodel = \"a.gguf\"\nmmproj = \"mmproj.gguf\"",
        Some("draft.gguf"),
        Some("org/repo@abc:draft.gguf"),
        "org/repo@abc:a.gguf+org/repo@abc:mmproj.gguf+org/repo@abc:draft.gguf"
    )]
    #[case::relative_files(
        "source = \"relative_files\"\nmodel = \"m/a.gguf\"\nmmproj = \"m/mmproj.gguf\"",
        Some("m/draft.gguf"),
        Some("m/draft.gguf"),
        "m/a.gguf+m/mmproj.gguf+m/draft.gguf"
    )]
    #[case::absolute_files(
        "source = \"absolute_files\"\nmodel = \"/m/a.gguf\"\nmmproj = \"/m/mmproj.gguf\"",
        Some("/m/draft.gguf"),
        Some("/m/draft.gguf"),
        "/m/a.gguf+/m/mmproj.gguf+/m/draft.gguf"
    )]
    #[case::url(
        "source = \"url\"\nmodel = \"https://cdn.example/m.gguf\"\nmmproj = \"https://cdn.example/mmproj.gguf\"",
        Some("https://cdn.example/draft.gguf"),
        Some("https://cdn.example/draft.gguf"),
        "https://cdn.example/m.gguf+https://cdn.example/mmproj.gguf+https://cdn.example/draft.gguf"
    )]
    #[case::vault_url(
        "source = \"vault_url\"\nmodel = \"/vault/resources/vault_1/content\"\nmmproj = \"/vault/resources/vault_2/content\"\nprincipal_id = \"org_01936d\"",
        Some("/vault/resources/vault_3/content"),
        Some("/vault/resources/vault_3/content"),
        "/vault/resources/vault_1/content+/vault/resources/vault_2/content+/vault/resources/vault_3/content"
    )]
    #[case::huggingface_merged(
        "source = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\nmodel = \"merged.gguf\"\nmmproj = \"mmproj.gguf\"",
        None,
        None,
        "org/repo:merged.gguf+org/repo:mmproj.gguf"
    )]
    #[case::relative_files_merged(
        "source = \"relative_files\"\nmodel = \"m/merged.gguf\"\nmmproj = \"m/mmproj.gguf\"",
        None,
        None,
        "m/merged.gguf+m/mmproj.gguf"
    )]
    fn gguf_vision_speculative_names_every_file(
        #[case] files: &str,
        #[case] draft: Option<&str>,
        #[case] draft_reference: Option<&str>,
        #[case] identity: &str,
    ) -> anyhow::Result<()> {
        let draft = draft
            .map(|d| format!("\ndraft = \"{d}\""))
            .unwrap_or_default();
        let model: Model = toml::from_str(&format!(
            "type = \"gguf_vision_speculative\"\n{files}{draft}"
        ))
        .context("gguf_vision_speculative should parse")?;
        let Model::GgufVisionSpeculative(m) = &model else {
            anyhow::bail!("expected GgufVisionSpeculative, got {model:?}");
        };
        assert_eq!(
            m.source.vision(),
            toml::from_str::<GgufVisionSource>(files)?
        );
        assert_eq!(m.source.draft_reference().as_deref(), draft_reference);
        assert_eq!(ModelType::of(&model), ModelType::GgufVisionSpeculative);
        assert!(model.auth_token().is_none());
        assert_eq!(model.to_string(), identity);
        Ok(())
    }

    /// A merged speculative model is a `gguf_vision` body under another `type`:
    /// the shared per-file structs keep the two from drifting.
    #[rstest::rstest]
    #[case::huggingface(
        "source = \"huggingface\"\norg = \"org\"\nrepo_name = \"repo\"\nrevision = \"abc\"\nmodel = \"a.gguf\"\nmodel_sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nmmproj = \"mmproj.gguf\""
    )]
    #[case::relative_files(
        "source = \"relative_files\"\nmodel = \"m/a.gguf\"\nmmproj = \"m/mmproj.gguf\""
    )]
    #[case::absolute_files(
        "source = \"absolute_files\"\nmodel = \"/m/a.gguf\"\nmmproj = \"/m/mmproj.gguf\""
    )]
    #[case::url(
        "source = \"url\"\nmodel = \"https://cdn.example/m.gguf\"\nmmproj = \"https://cdn.example/mmproj.gguf\"\nmmproj_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\""
    )]
    fn gguf_vision_and_speculative_share_the_file_shape(
        #[case] source: &str,
    ) -> anyhow::Result<()> {
        let vision: Model = toml::from_str(&format!("type = \"gguf_vision\"\n{source}"))?;
        let speculative: Model =
            toml::from_str(&format!("type = \"gguf_vision_speculative\"\n{source}"))?;
        assert_eq!(vision.to_string(), speculative.to_string());

        let without_type = |model: &Model| -> anyhow::Result<serde_json::Value> {
            let mut value = serde_json::to_value(model)?;
            value
                .as_object_mut()
                .context("a model serializes to an object")?
                .remove("type");
            Ok(value)
        };
        assert_eq!(without_type(&vision)?, without_type(&speculative)?);
        [vision, speculative].iter().try_for_each(|model| {
            assert_eq!(&toml::from_str::<Model>(&toml::to_string(model)?)?, model);
            assert_eq!(
                &serde_json::from_str::<Model>(&serde_json::to_string(model)?)?,
                model
            );
            anyhow::Ok(())
        })
    }

    /// A vault fetch is its own tag, and the principal is part of the row.
    #[test]
    fn a_vault_url_carries_its_principal_and_its_own_tag() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"gguf_text\"\nsource = \"vault_url\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\"",
        )
        .context("vault_url gguf_text should parse")?;
        let Model::GgufText(m) = &model else {
            anyhow::bail!("expected GgufText");
        };
        let GgufTextSource::VaultUrl { principal_id, .. } = &m.source else {
            anyhow::bail!("expected a vault url");
        };
        assert_eq!(principal_id.to_string(), "org_01936d");
        assert!(model.auth_token().is_none(), "mgmt holds the credential");
        assert_eq!(serde_json::to_value(&model)?["source"], "vault_url");
        Ok(())
    }
}
