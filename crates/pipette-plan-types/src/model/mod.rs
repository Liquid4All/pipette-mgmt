//! [`Model`], its per-format variant structs and the sources each is fetched
//! from; the gguf kinds live in `gguf`.

mod gguf;

pub use gguf::{
    GgufText, GgufTextSource, GgufVision, GgufVisionAbsoluteFiles, GgufVisionHfFiles,
    GgufVisionRelativeFiles, GgufVisionSource, GgufVisionSpeculative, GgufVisionSpeculativeSource,
    GgufVisionUrlFiles, GgufVisionVaultUrlFiles,
};

use serde::{Deserialize, Serialize};

use crate::{
    AbsolutePath, AuthToken, HfRepo, NonEmptyTrimmedString, RelativePath, RepoSubpath, Sha256,
    VaultPath, VaultPrincipalId,
};

/// One model deployment, tagged by artifact format.
///
/// TOML authoring shape is a tagged table with `type`:
///
/// ```toml
/// models = [
///   { type = "gguf_text",   source = "huggingface", org = "meta-llama", repo_name = "llama-3.2-1b", path = "Q4_K_M.gguf" },
///   { type = "gguf_vision", source = "huggingface", org = "example-org", repo_name = "VLM-3B-GGUF", model = "q4_K_M.gguf", mmproj = "mmproj-f16.gguf" },
///   { type = "gguf_vision_speculative", source = "huggingface", org = "example-org", repo_name = "VLM-3B-Draft-GGUF", model = "merged-f16.gguf", mmproj = "mmproj-f16.gguf" },
///   { type = "mlx",         source = "huggingface", org = "LiquidAI",   repo_name = "LFM2.5-350M-MLX-4bit" },
///   { type = "mlx_speculative", target = { source = "huggingface", org = "example-org", repo_name = "VLM-3B-MLX-8bit" }, draft = { source = "huggingface", org = "example-org", repo_name = "VLM-3B-Draft-MLX" } },
///   { type = "torch",       source = "huggingface", org = "meta-llama", repo_name = "Llama-3.2-1B" },
///   { type = "torch_speculative", target = { source = "huggingface", org = "example-org", repo_name = "VLM-3B" }, draft = { source = "huggingface", org = "example-org", repo_name = "VLM-3B-Draft" } },
///   { type = "openvino",    source = "huggingface", org = "LiquidAI",   repo_name = "LFM2.5-350M-ov", prefix = "int4-sym-cw" },
///   { type = "onnx_genai",  source = "huggingface", org = "microsoft",  repo_name = "Phi-3-mini-4k-instruct-onnx", prefix = "cpu_and_mobile/cpu-int4-rtn-block-32" },
/// ]
/// ```
///
/// The serving backend (vLLM, sglang, …) is the `Runtime`'s job, not the model's.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Model {
    GgufText(GgufText),
    GgufVision(GgufVision),
    GgufVisionSpeculative(GgufVisionSpeculative),
    Mlx(Mlx),
    MlxSpeculative(MlxSpeculative),
    Torch(Torch),
    TorchSpeculative(TorchSpeculative),
    Openvino(Openvino),
    OnnxGenai(OnnxGenai),
    /// Apple Foundation Models, text variant: a bare marker, since the model
    /// ships with the OS.
    AppleFoundationText,
}

/// Where a directory-style model lives; the artifact format is the enclosing
/// [`Model`] variant's concern.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ModelSource {
    #[serde(rename = "huggingface")]
    HuggingFace {
        #[serde(flatten)]
        repo: HfRepo,
        /// Subdirectory within the repo the model lives in; `None` = repo root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<RepoSubpath>,
    },
    /// The directory packed as one archive (tar, tar.gz or zip), unpacked as
    /// is: the archive's root is the model directory.
    VaultArchive {
        url: VaultPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<Sha256>,
        principal_id: VaultPrincipalId,
        /// The vault record's `name`, which ingestion checks: a path says only an id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
        custom_label: Option<NonEmptyTrimmedString>,
    },
    /// Portable relative directory.
    RelativeDir { dir: RelativePath },
    /// Absolute host directory.
    AbsoluteDir { dir: AbsolutePath },
}

impl ModelSource {
    /// Identity string: for HF, `org/repo[@revision][:prefix]`; otherwise the
    /// path.
    pub fn reference(&self) -> String {
        match self {
            ModelSource::HuggingFace { repo, prefix } => match prefix {
                Some(prefix) => format!("{}:{prefix}", repo.reference()),
                None => repo.reference(),
            },
            ModelSource::VaultArchive { url, .. } => url.to_string(),
            ModelSource::RelativeDir { dir } => dir.to_string(),
            ModelSource::AbsoluteDir { dir } => dir.to_string(),
        }
    }

    pub fn auth_token(&self) -> Option<&AuthToken> {
        match self {
            ModelSource::HuggingFace { repo, .. } => repo.auth_token.as_ref(),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        }
    }
}

/// MLX bundle (quantized safetensors + config), directory-shaped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Mlx {
    #[serde(flatten)]
    pub source: ModelSource,
}

/// Speculative MLX pair. A model type of its own, not a flag on [`Mlx`]: the
/// drafter is part of what was measured, so it must be part of the identity.
pub type MlxSpeculative = SpeculativePair;

/// Speculative torch pair: the target a vLLM/SGLang server loads and the
/// drafter repository it is launched with.
pub type TorchSpeculative = SpeculativePair;

/// A target and the drafter that proposes for it, each a repository of its own.
/// The server takes the drafter as a second path, so there is no merged form
/// and `draft` is required.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SpeculativePair {
    pub target: ModelSource,
    pub draft: ModelSource,
}

impl SpeculativePair {
    /// Both sources, in the order the store lays them out.
    pub fn sources(&self) -> [&ModelSource; 2] {
        [&self.target, &self.draft]
    }

    pub fn sources_mut(&mut self) -> [&mut ModelSource; 2] {
        [&mut self.target, &mut self.draft]
    }
}

/// PyTorch / HF Transformers safetensors weights, directory-shaped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Torch {
    #[serde(flatten)]
    pub source: ModelSource,
}

/// OpenVINO IR bundle, directory-shaped, including the tokenizer and
/// detokenizer IR pairs it generates with. Weight precision is part of the
/// authored coordinate (repo name or `prefix`); the directory does not record it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Openvino {
    #[serde(flatten)]
    pub source: ModelSource,
}

/// ONNX Runtime GenAI model directory: graph, weights, `genai_config.json` and
/// tokenizer files. Weight precision is part of the authored coordinate, usually
/// a `prefix` subdirectory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OnnxGenai {
    #[serde(flatten)]
    pub source: ModelSource,
}

impl Model {
    /// The access token this model carries for fetching, if any.
    pub fn auth_token(&self) -> Option<&AuthToken> {
        match self {
            Model::GgufText(m) => m.source.auth_token(),
            Model::GgufVision(m) => m.source.auth_token(),
            Model::GgufVisionSpeculative(m) => m.source.auth_token(),
            Model::Mlx(m) => m.source.auth_token(),
            // One of the two; each repo is fetched with its own token.
            Model::MlxSpeculative(m) | Model::TorchSpeculative(m) => {
                m.target.auth_token().or_else(|| m.draft.auth_token())
            }
            Model::Torch(m) => m.source.auth_token(),
            Model::Openvino(m) => m.source.auth_token(),
            Model::OnnxGenai(m) => m.source.auth_token(),
            Model::AppleFoundationText => None,
        }
    }

    /// A copy with every source's auth token cleared, since a persisted
    /// manifest must never carry a secret.
    pub fn without_auth_token(&self) -> Model {
        let mut model = self.clone();
        match &mut model {
            Model::GgufText(m) => {
                if let GgufTextSource::HuggingFace { repo, .. } = &mut m.source {
                    repo.auth_token = None;
                }
            }
            Model::GgufVision(m) => {
                if let GgufVisionSource::HuggingFace(f) = &mut m.source {
                    f.repo.auth_token = None;
                }
            }
            Model::GgufVisionSpeculative(m) => {
                if let GgufVisionSpeculativeSource::HuggingFace { files, .. } = &mut m.source {
                    files.repo.auth_token = None;
                }
            }
            Model::Mlx(m) => {
                if let ModelSource::HuggingFace { repo, .. } = &mut m.source {
                    repo.auth_token = None;
                }
            }
            // Both, not just the one `auth_token` returns.
            Model::MlxSpeculative(m) | Model::TorchSpeculative(m) => {
                m.sources_mut().into_iter().for_each(|source| {
                    if let ModelSource::HuggingFace { repo, .. } = source {
                        repo.auth_token = None;
                    }
                });
            }
            Model::Torch(m) => {
                if let ModelSource::HuggingFace { repo, .. } = &mut m.source {
                    repo.auth_token = None;
                }
            }
            Model::Openvino(m) => {
                if let ModelSource::HuggingFace { repo, .. } = &mut m.source {
                    repo.auth_token = None;
                }
            }
            Model::OnnxGenai(m) => {
                if let ModelSource::HuggingFace { repo, .. } = &mut m.source {
                    repo.auth_token = None;
                }
            }
            Model::AppleFoundationText => {}
        }
        model
    }
}

/// Inject `token` into every HF source that carries none yet; an explicit token
/// wins. Returns whether it was applied.
pub fn inject_hf_auth_token(model: &mut Model, token: AuthToken) -> bool {
    // Two repos: the single-repo match below would miss the drafter.
    if let Model::MlxSpeculative(m) | Model::TorchSpeculative(m) = model {
        return m
            .sources_mut()
            .into_iter()
            .filter_map(|source| match source {
                ModelSource::HuggingFace { repo, .. } => Some(repo),
                ModelSource::VaultArchive { .. }
                | ModelSource::RelativeDir { .. }
                | ModelSource::AbsoluteDir { .. } => None,
            })
            .filter(|repo| repo.auth_token.is_none())
            .map(|repo| repo.auth_token = Some(token.clone()))
            .count()
            > 0;
    }
    let repo = match model {
        Model::GgufText(m) => match &mut m.source {
            GgufTextSource::HuggingFace { repo, .. } => Some(repo),
            GgufTextSource::RelativeFile { .. }
            | GgufTextSource::AbsoluteFile { .. }
            | GgufTextSource::Url { .. }
            | GgufTextSource::VaultUrl { .. } => None,
        },
        Model::GgufVision(m) => match &mut m.source {
            GgufVisionSource::HuggingFace(f) => Some(&mut f.repo),
            GgufVisionSource::RelativeFiles(_)
            | GgufVisionSource::AbsoluteFiles(_)
            | GgufVisionSource::Url(_)
            | GgufVisionSource::VaultUrl(_) => None,
        },
        Model::GgufVisionSpeculative(m) => match &mut m.source {
            GgufVisionSpeculativeSource::HuggingFace { files, .. } => Some(&mut files.repo),
            GgufVisionSpeculativeSource::RelativeFiles { .. }
            | GgufVisionSpeculativeSource::AbsoluteFiles { .. }
            | GgufVisionSpeculativeSource::Url { .. }
            | GgufVisionSpeculativeSource::VaultUrl { .. } => None,
        },
        Model::Mlx(m) => match &mut m.source {
            ModelSource::HuggingFace { repo, .. } => Some(repo),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        },
        Model::Torch(m) => match &mut m.source {
            ModelSource::HuggingFace { repo, .. } => Some(repo),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        },
        Model::Openvino(m) => match &mut m.source {
            ModelSource::HuggingFace { repo, .. } => Some(repo),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        },
        Model::OnnxGenai(m) => match &mut m.source {
            ModelSource::HuggingFace { repo, .. } => Some(repo),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        },
        // Handled above: two repos, not one.
        Model::MlxSpeculative(_) | Model::TorchSpeculative(_) | Model::AppleFoundationText => None,
    };
    match repo {
        Some(repo) if repo.auth_token.is_none() => {
            repo.auth_token = Some(token);
            true
        }
        _ => false,
    }
}

impl std::fmt::Display for Mlx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.reference())
    }
}

impl std::fmt::Display for SpeculativePair {
    /// `{target}+{drafter}`, the same join [`GgufVisionSpeculative`] uses.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}+{}", self.target.reference(), self.draft.reference())
    }
}

impl std::fmt::Display for Torch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.reference())
    }
}

impl std::fmt::Display for Openvino {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.reference())
    }
}

impl std::fmt::Display for OnnxGenai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.reference())
    }
}

/// `{repo}[:{path}]`, the operator-facing string for logs and listings, and
/// what `pipette-ctl` hashes into a cell's resume key. Not the wire form.
impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Model::GgufText(m) => m.fmt(f),
            Model::GgufVision(m) => m.fmt(f),
            Model::GgufVisionSpeculative(m) => m.fmt(f),
            Model::Mlx(m) => m.fmt(f),
            Model::MlxSpeculative(m) | Model::TorchSpeculative(m) => m.fmt(f),
            Model::Torch(m) => m.fmt(f),
            Model::Openvino(m) => m.fmt(f),
            Model::OnnxGenai(m) => m.fmt(f),
            // Matches the AFM client's submitted `model_name`.
            Model::AppleFoundationText => write!(f, "apple/foundation-text"),
        }
    }
}

/// Model-type discriminant, mirroring the [`Model`] enum's variants by name.
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
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(any(test, feature = "schemars"), derive(strum::EnumIter))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ModelType {
    GgufText,
    GgufVision,
    GgufVisionSpeculative,
    Mlx,
    MlxSpeculative,
    Torch,
    TorchSpeculative,
    Openvino,
    OnnxGenai,
    AppleFoundationText,
}

impl ModelType {
    /// The kind of a concrete [`Model`].
    pub fn of(model: &Model) -> Self {
        match model {
            Model::GgufText(_) => Self::GgufText,
            Model::GgufVision(_) => Self::GgufVision,
            Model::GgufVisionSpeculative(_) => Self::GgufVisionSpeculative,
            Model::Mlx(_) => Self::Mlx,
            Model::MlxSpeculative(_) => Self::MlxSpeculative,
            Model::Torch(_) => Self::Torch,
            Model::TorchSpeculative(_) => Self::TorchSpeculative,
            Model::Openvino(_) => Self::Openvino,
            Model::OnnxGenai(_) => Self::OnnxGenai,
            Model::AppleFoundationText => Self::AppleFoundationText,
        }
    }

    /// Whether the type pairs a model with a draft model.
    pub fn is_speculative(self) -> bool {
        match self {
            Self::GgufVisionSpeculative | Self::MlxSpeculative | Self::TorchSpeculative => true,
            Self::GgufText
            | Self::GgufVision
            | Self::Mlx
            | Self::Torch
            | Self::Openvino
            | Self::OnnxGenai
            | Self::AppleFoundationText => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;
    use crate::plan::plan_toml;
    use crate::runtime::cli_release;
    use crate::*;

    fn org(s: &str) -> anyhow::Result<HfOrg> {
        Ok(HfOrg::try_new(s.to_owned())?)
    }
    fn repo_name(s: &str) -> anyhow::Result<HfRepoName> {
        Ok(HfRepoName::try_new(s.to_owned())?)
    }
    fn bench(s: &str) -> anyhow::Result<BenchmarkId> {
        Ok(BenchmarkId::try_new(s.to_owned())?)
    }

    /// The two kinds share one payload, so only the `type` tag tells them apart.
    #[rstest::rstest]
    #[case::mlx("mlx_speculative", ModelType::MlxSpeculative)]
    #[case::torch("torch_speculative", ModelType::TorchSpeculative)]
    fn a_speculative_pair_names_both_sources(
        #[case] kind: &str,
        #[case] model_type: ModelType,
    ) -> anyhow::Result<()> {
        let model: Model = toml::from_str(&format!(
            "type = \"{kind}\"\ntarget = {{ source = \"huggingface\", org = \"o\", repo_name = \"target\", prefix = \"4bit\" }}\ndraft = {{ source = \"huggingface\", org = \"o\", repo_name = \"draft\" }}"
        ))?;
        assert_eq!(ModelType::of(&model), model_type);
        assert_eq!(model_type.to_string(), kind);
        assert_eq!(model.to_string(), "o/target:4bit+o/draft");
        assert_eq!(
            serde_json::to_value(&model)?,
            serde_json::json!({
                "type": kind,
                "target": {"source": "huggingface", "org": "o", "repo_name": "target", "prefix": "4bit"},
                "draft": {"source": "huggingface", "org": "o", "repo_name": "draft"},
            })
        );
        assert_eq!(toml::from_str::<Model>(&toml::to_string(&model)?)?, model);
        Ok(())
    }

    #[test]
    fn a_pairs_sources_run_target_then_draft() -> anyhow::Result<()> {
        let mut pair: SpeculativePair = toml::from_str(
            "target = { source = \"huggingface\", org = \"o\", repo_name = \"target\" }\ndraft = { source = \"relative_dir\", dir = \"drafts/d\" }",
        )?;
        assert_eq!(
            pair.sources().map(ModelSource::reference),
            ["o/target", "drafts/d"]
        );

        let dir = |path: &str| -> anyhow::Result<ModelSource> {
            Ok(ModelSource::RelativeDir {
                dir: RelativePath::try_new(path.to_owned())?,
            })
        };
        let [target, draft] = pair.sources_mut();
        *target = dir("t")?;
        *draft = dir("d")?;
        assert_eq!(
            [pair.target.reference(), pair.draft.reference()],
            ["t", "d"]
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::gguf_text(
        "type = \"gguf_text\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\npath = \"m.gguf\""
    )]
    #[case::gguf_vision(
        "type = \"gguf_vision\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\nmodel = \"m.gguf\"\nmmproj = \"p.gguf\""
    )]
    #[case::mlx("type = \"mlx\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"")]
    #[case::torch("type = \"torch\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"")]
    #[case::openvino(
        "type = \"openvino\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\""
    )]
    #[case::onnx_genai(
        "type = \"onnx_genai\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\""
    )]
    #[case::mlx_speculative(
        "type = \"mlx_speculative\"\ntarget = { source = \"huggingface\", org = \"o\", repo_name = \"t\" }\ndraft = { source = \"huggingface\", org = \"o\", repo_name = \"d\" }"
    )]
    #[case::torch_speculative(
        "type = \"torch_speculative\"\ntarget = { source = \"huggingface\", org = \"o\", repo_name = \"t\" }\ndraft = { source = \"huggingface\", org = \"o\", repo_name = \"d\" }"
    )]
    #[case::gguf_vision_speculative(
        "type = \"gguf_vision_speculative\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\nmodel = \"m.gguf\"\nmmproj = \"p.gguf\"\ndraft = \"d.gguf\""
    )]
    fn every_hf_kind_carries_an_injected_token(#[case] model: &str) -> anyhow::Result<()> {
        let mut model: Model = toml::from_str(model)?;
        let token = AuthToken::try_new("hf_secret".to_owned())?;
        assert!(inject_hf_auth_token(&mut model, token.clone()));
        assert_eq!(model.auth_token(), Some(&token));
        assert!(model.without_auth_token().auth_token().is_none());
        Ok(())
    }

    #[test]
    fn openvino_prefix_selects_a_precision_variant() -> anyhow::Result<()> {
        // `prefix` must fold into the identity, or precision variants collide.
        let model: Model = toml::from_str(
            "type = \"openvino\"\nsource = \"huggingface\"\norg = \"LiquidAI\"\nrepo_name = \"LFM2.5-350M-ov\"\nprefix = \"int4-sym-cw\"",
        )
        .context("openvino with prefix should parse")?;
        assert_eq!(ModelType::of(&model), ModelType::Openvino);
        assert_eq!(model.to_string(), "LiquidAI/LFM2.5-350M-ov:int4-sym-cw");
        Ok(())
    }

    #[test]
    fn openvino_from_local_dir_parses_needs_no_auth() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"openvino\"\nsource = \"absolute_dir\"\ndir = \"/models/lfm2.5-350m-int4-sym-cw\"",
        )?;
        let Model::Openvino(m) = &model else {
            anyhow::bail!("expected Openvino, got {model:?}");
        };
        assert!(matches!(m.source, ModelSource::AbsoluteDir { .. }));
        assert!(model.auth_token().is_none());
        Ok(())
    }

    #[test]
    fn mlx_pins_revision_and_folds_it_into_identity() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"mlx\"\nsource = \"huggingface\"\norg = \"LiquidAI\"\nrepo_name = \"LFM2.5-350M-MLX-4bit\"\nrevision = \"v2\"",
        )
        .context("mlx with revision should parse")?;
        let Model::Mlx(m) = &model else {
            anyhow::bail!("expected Mlx");
        };
        let ModelSource::HuggingFace { repo, .. } = &m.source else {
            anyhow::bail!("expected HuggingFace source");
        };
        assert_eq!(
            repo.revision.as_ref().map(|r| r.to_string()),
            Some("v2".to_owned())
        );
        assert_eq!(model.to_string(), "LiquidAI/LFM2.5-350M-MLX-4bit@v2");
        // Absent revision stays the bare id, as stored rows have it.
        let bare: Model = toml::from_str(
            "type = \"mlx\"\nsource = \"huggingface\"\norg = \"LiquidAI\"\nrepo_name = \"LFM2.5-350M-MLX-4bit\"",
        )?;
        assert_eq!(bare.to_string(), "LiquidAI/LFM2.5-350M-MLX-4bit");
        Ok(())
    }

    #[test]
    fn mlx_from_local_dir_parses_needs_no_auth() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"mlx\"\nsource = \"absolute_dir\"\ndir = \"/models/LFM2.5-350M-MLX-4bit\"",
        )
        .context("abs_local mlx should parse")?;
        let Model::Mlx(m) = &model else {
            anyhow::bail!("expected Mlx");
        };
        assert!(matches!(m.source, ModelSource::AbsoluteDir { .. }));
        assert!(m.source.auth_token().is_none());
        assert_eq!(model.to_string(), "/models/LFM2.5-350M-MLX-4bit");
        Ok(())
    }

    #[test]
    fn mlx_hf_prefix_folds_into_identity() -> anyhow::Result<()> {
        let model: Model = toml::from_str(
            "type = \"mlx\"\nsource = \"huggingface\"\norg = \"mlx-community\"\nrepo_name = \"bundle\"\nprefix = \"4bit\"",
        )
        .context("mlx with prefix should parse")?;
        assert_eq!(model.to_string(), "mlx-community/bundle:4bit");
        Ok(())
    }

    #[test]
    fn torch_from_hf_and_local_parse() -> anyhow::Result<()> {
        let hf: Model =
            toml::from_str("type = \"torch\"\nsource = \"huggingface\"\norg = \"meta-llama\"\nrepo_name = \"Llama-3.2-1B\"")
                .context("hf torch should parse")?;
        let Model::Torch(m) = &hf else {
            anyhow::bail!("expected Torch");
        };
        assert!(matches!(m.source, ModelSource::HuggingFace { .. }));
        assert_eq!(hf.to_string(), "meta-llama/Llama-3.2-1B");

        let local: Model =
            toml::from_str("type = \"torch\"\nsource = \"absolute_dir\"\ndir = \"/models/llama\"")
                .context("abs_local torch should parse")?;
        let Model::Torch(m) = &local else {
            anyhow::bail!("expected Torch");
        };
        assert!(matches!(m.source, ModelSource::AbsoluteDir { .. }));
        assert!(local.auth_token().is_none());
        assert_eq!(local.to_string(), "/models/llama");
        Ok(())
    }

    #[test]
    fn inject_hf_auth_token_only_fills_a_gated_tokenless_source() -> anyhow::Result<()> {
        let token = || AuthToken::try_new("hf_injected".to_owned());

        let mut hf: Model = toml::from_str(
            "type = \"torch\"\nsource = \"huggingface\"\norg = \"meta-llama\"\nrepo_name = \"Llama-3.2-1B\"",
        )?;
        assert!(inject_hf_auth_token(&mut hf, token()?));
        assert_eq!(hf.auth_token().map(|t| t.as_ref()), Some("hf_injected"));

        // An explicit token wins.
        let mut owned: Model = toml::from_str(
            "type = \"torch\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\nauth_token = \"hf_declared\"",
        )?;
        assert!(!inject_hf_auth_token(&mut owned, token()?));
        assert_eq!(owned.auth_token().map(|t| t.as_ref()), Some("hf_declared"));
        Ok(())
    }

    /// A non-HF source takes no token, and its identity is its path, URL or route.
    #[rstest::rstest]
    #[case::gguf_text_relative(
        r#"{"type":"gguf_text","source":"relative_file","path":"m.gguf"}"#,
        "m.gguf"
    )]
    #[case::gguf_text_url(
        r#"{"type":"gguf_text","source":"url","url":"https://example.test/m.gguf"}"#,
        "https://example.test/m.gguf"
    )]
    #[case::gguf_text_vault(
        r#"{"type":"gguf_text","source":"vault_url","url":"/vault/resources/vault_1/content","principal_id":"org_1"}"#,
        "/vault/resources/vault_1/content"
    )]
    #[case::gguf_vision_relative(
        r#"{"type":"gguf_vision","source":"relative_files","model":"m.gguf","mmproj":"p.gguf"}"#,
        "m.gguf+p.gguf"
    )]
    #[case::gguf_vision_vault(
        r#"{"type":"gguf_vision","source":"vault_url","model":"/vault/resources/vault_1/content","mmproj":"/vault/resources/vault_2/content","principal_id":"org_1"}"#,
        "/vault/resources/vault_1/content+/vault/resources/vault_2/content"
    )]
    #[case::gguf_vision_speculative_relative(
        r#"{"type":"gguf_vision_speculative","source":"relative_files","model":"m.gguf","mmproj":"p.gguf","draft":"d.gguf"}"#,
        "m.gguf+p.gguf+d.gguf"
    )]
    #[case::mlx_vault(
        r#"{"type":"mlx","source":"vault_archive","url":"/vault/resources/vault_1/content","principal_id":"org_1"}"#,
        "/vault/resources/vault_1/content"
    )]
    #[case::torch_absolute(
        r#"{"type":"torch","source":"absolute_dir","dir":"/models/llama"}"#,
        "/models/llama"
    )]
    #[case::openvino_relative(
        r#"{"type":"openvino","source":"relative_dir","dir":"models/ov"}"#,
        "models/ov"
    )]
    #[case::onnx_genai_absolute(
        r#"{"type":"onnx_genai","source":"absolute_dir","dir":"/models/onnx"}"#,
        "/models/onnx"
    )]
    #[case::mlx_speculative_local(
        r#"{"type":"mlx_speculative","target":{"source":"relative_dir","dir":"t"},"draft":{"source":"absolute_dir","dir":"/d"}}"#,
        "t+/d"
    )]
    #[case::apple_foundation_text(r#"{"type":"apple_foundation_text"}"#, "apple/foundation-text")]
    fn a_source_outside_huggingface_takes_no_token(
        #[case] json: &str,
        #[case] identity: &str,
    ) -> anyhow::Result<()> {
        let authored: Model = serde_json::from_str(json)?;
        let mut model = authored.clone();
        assert!(!inject_hf_auth_token(
            &mut model,
            AuthToken::try_new("hf_injected".to_owned())?
        ));
        assert_eq!(model, authored);
        assert!(model.auth_token().is_none());
        assert_eq!(model.without_auth_token(), authored);
        assert_eq!(model.to_string(), identity);
        Ok(())
    }

    #[rstest::rstest]
    #[case::gguf_text(r#"{"type":"gguf_text","source":"relative_file","path":"m.gguf"}"#)]
    #[case::gguf_vision(
        r#"{"type":"gguf_vision","source":"relative_files","model":"m.gguf","mmproj":"p.gguf"}"#
    )]
    #[case::gguf_vision_speculative(
        r#"{"type":"gguf_vision_speculative","source":"relative_files","model":"m.gguf","mmproj":"p.gguf"}"#
    )]
    #[case::mlx(r#"{"type":"mlx","source":"relative_dir","dir":"m"}"#)]
    #[case::mlx_speculative(
        r#"{"type":"mlx_speculative","target":{"source":"relative_dir","dir":"t"},"draft":{"source":"relative_dir","dir":"d"}}"#
    )]
    #[case::torch(r#"{"type":"torch","source":"relative_dir","dir":"m"}"#)]
    #[case::torch_speculative(
        r#"{"type":"torch_speculative","target":{"source":"relative_dir","dir":"t"},"draft":{"source":"relative_dir","dir":"d"}}"#
    )]
    #[case::openvino(r#"{"type":"openvino","source":"relative_dir","dir":"m"}"#)]
    #[case::onnx_genai(r#"{"type":"onnx_genai","source":"relative_dir","dir":"m"}"#)]
    #[case::apple_foundation_text(r#"{"type":"apple_foundation_text"}"#)]
    fn model_type_is_spelled_as_the_type_tag(#[case] json: &str) -> anyhow::Result<()> {
        let model: Model = serde_json::from_str(json)?;
        let tag = serde_json::to_value(&model)?["type"].clone();
        let kind = ModelType::of(&model);
        assert_eq!(serde_json::to_value(kind)?, tag);
        assert_eq!(serde_json::from_value::<ModelType>(tag.clone())?, kind);
        assert_eq!(Some(kind.to_string().as_str()), tag.as_str());
        Ok(())
    }

    /// The drafter is usually the gated repo, so injecting into one would 404 it.
    #[test]
    fn inject_hf_auth_token_fills_both_halves_of_a_pair() -> anyhow::Result<()> {
        let mut pair = Model::MlxSpeculative(MlxSpeculative {
            target: ModelSource::HuggingFace {
                repo: HfRepo {
                    org: org("LiquidAI")?,
                    repo_name: repo_name("vl-mlx")?,
                    revision: None,
                    auth_token: None,
                },
                prefix: None,
            },
            draft: ModelSource::HuggingFace {
                repo: HfRepo {
                    org: org("LiquidAI")?,
                    repo_name: repo_name("vl-dspark")?,
                    revision: None,
                    auth_token: None,
                },
                prefix: None,
            },
        });

        let tokens = |m: &Model| -> Vec<Option<String>> {
            let Model::MlxSpeculative(m) = m else {
                return Vec::new();
            };
            m.sources()
                .into_iter()
                .map(|s| match s {
                    ModelSource::HuggingFace { repo, .. } => {
                        repo.auth_token.as_ref().map(|t| t.as_ref().to_owned())
                    }
                    ModelSource::VaultArchive { .. }
                    | ModelSource::RelativeDir { .. }
                    | ModelSource::AbsoluteDir { .. } => None,
                })
                .collect()
        };

        assert!(inject_hf_auth_token(
            &mut pair,
            AuthToken::try_new("hf_injected".to_owned())?
        ));
        assert_eq!(
            tokens(&pair),
            vec![
                Some("hf_injected".to_owned()),
                Some("hf_injected".to_owned())
            ],
            "a drafter the fetch cannot authenticate for fails halfway"
        );

        let clean = pair.without_auth_token();
        assert_eq!(tokens(&clean), vec![None, None]);
        assert!(clean.auth_token().is_none());
        Ok(())
    }

    #[test]
    fn inject_hf_auth_token_fills_only_the_tokenless_half() -> anyhow::Result<()> {
        let hf = |repo: &str, token: Option<&str>| -> anyhow::Result<ModelSource> {
            Ok(ModelSource::HuggingFace {
                repo: HfRepo {
                    org: org("LiquidAI")?,
                    repo_name: repo_name(repo)?,
                    revision: None,
                    auth_token: token
                        .map(|t| AuthToken::try_new(t.to_owned()))
                        .transpose()?,
                },
                prefix: None,
            })
        };
        let mut pair = Model::MlxSpeculative(MlxSpeculative {
            target: hf("vl-mlx", Some("hf_declared"))?,
            draft: hf("vl-dspark", None)?,
        });

        assert!(inject_hf_auth_token(
            &mut pair,
            AuthToken::try_new("hf_injected".to_owned())?
        ));
        let Model::MlxSpeculative(m) = &pair else {
            anyhow::bail!("expected MlxSpeculative");
        };
        let token = |s: &ModelSource| match s {
            ModelSource::HuggingFace { repo, .. } => {
                repo.auth_token.as_ref().map(|t| t.as_ref().to_owned())
            }
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        };
        assert_eq!(token(&m.target), Some("hf_declared".to_owned()));
        assert_eq!(token(&m.draft), Some("hf_injected".to_owned()));
        Ok(())
    }

    #[test]
    fn hf_model_round_trips_through_toml() -> anyhow::Result<()> {
        let model = Model::GgufText(GgufText {
            source: GgufTextSource::HuggingFace {
                repo: HfRepo {
                    org: org("meta-llama")?,
                    repo_name: repo_name("llama-3.2-1b")?,
                    revision: None,
                    auth_token: Some(AuthToken::try_new("hf_test_xxx".to_owned())?),
                },
                path: RepoSubpath::try_new("Q4_K_M.gguf".to_owned())?,
                sha256: None,
            },
        });
        let s = toml::to_string(&model)?;
        let round: Model = toml::from_str(&s)?;
        assert_eq!(model, round);
        Ok(())
    }

    #[test]
    fn local_model_round_trips_through_toml() -> anyhow::Result<()> {
        let model = Model::Mlx(Mlx {
            source: ModelSource::AbsoluteDir {
                dir: AbsolutePath::try_new("/models/x".to_owned())?,
            },
        });
        let s = toml::to_string(&model)?;
        let round: Model = toml::from_str(&s)?;
        assert_eq!(model, round);
        Ok(())
    }

    #[test]
    fn plan_round_trips_through_toml() -> anyhow::Result<()> {
        let liquid = HfRepo {
            org: org("LiquidAI")?,
            repo_name: repo_name("LFM2.5-350M-MLX-4bit")?,
            revision: None,
            auth_token: None,
        };
        let meta = HfRepo {
            org: org("meta-llama")?,
            repo_name: repo_name("llama-3.2-1b")?,
            revision: None,
            auth_token: Some(AuthToken::try_new("hf_test_xxx".to_owned())?),
        };
        let vision = HfRepo {
            org: org("LiquidAI")?,
            repo_name: repo_name("LFM2.5-Vision-3B")?,
            revision: None,
            auth_token: None,
        };

        let all_models = NonEmptyVec::try_new(vec![
            Model::GgufText(GgufText {
                source: GgufTextSource::HuggingFace {
                    repo: meta.clone(),
                    path: RepoSubpath::try_new("Q4_K_M.gguf".to_owned())?,
                    sha256: None,
                },
            }),
            Model::GgufVision(GgufVision {
                source: GgufVisionSource::HuggingFace(GgufVisionHfFiles {
                    repo: vision,
                    model: RepoSubpath::try_new("q4_K_M.gguf".to_owned())?,
                    model_sha256: None,
                    mmproj: RepoSubpath::try_new("mmproj-f16.gguf".to_owned())?,
                    mmproj_sha256: None,
                }),
            }),
            Model::Mlx(Mlx {
                source: ModelSource::HuggingFace {
                    repo: liquid.clone(),
                    prefix: None,
                },
            }),
            Model::Torch(Torch {
                source: ModelSource::HuggingFace {
                    repo: meta,
                    prefix: None,
                },
            }),
        ])?;

        let runtimes = [
            Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
                source: LlamacppCliStockToolsSource::GithubRelease(cli_release("b5000")?),
                flavor: LlamaCppFlavor::WindowsX64Vulkan,
            }),
            Runtime::MlxMacosPipette(MlxMacosPipette {
                version: NonEmptyString::try_new("0.20.0".to_owned())?,
                flavor: MlxMacosPipetteFlavor::MacosArm64,
                source: UvRuntimeSource::PipRequirementsText {
                    contents: NonEmptyString::try_new("mlx-lm==0.20.0\n".to_owned())?,
                    install_flags: None,
                },
            }),
            Runtime::DockerVllm(DockerVllm {
                image_name: NonEmptyString::try_new("vllm/vllm-openai".to_owned())?,
                image_tag: NonEmptyString::try_new("0.7.3".to_owned())?,
                flavor: VllmFlavor::NvidiaGpu,
            }),
            Runtime::DockerSglang(DockerSglang {
                image_name: NonEmptyString::try_new("lmsysorg/sglang".to_owned())?,
                image_tag: NonEmptyString::try_new("latest-cu130-runtime".to_owned())?,
                flavor: SglangFlavor::NvidiaGpu,
            }),
            Runtime::UvVllm(UvVllm {
                server_version: UvServerVersion::try_new("0.10.0".to_owned())?,
                build: UvBuild::try_new("cu121".to_owned())?,
                python_version: UvPythonVersion::try_new("3.12".to_owned())?,
                source: UvRuntimeSource::PipRequirementsText {
                    contents: NonEmptyString::try_new("vllm==0.10.0\ntorch==2.5.0\n".to_owned())?,
                    install_flags: None,
                },
            }),
            Runtime::UvSglang(UvSglang {
                server_version: UvServerVersion::try_new("0.4.0".to_owned())?,
                build: UvBuild::try_new("cu121".to_owned())?,
                python_version: UvPythonVersion::try_new("3.12".to_owned())?,
                source: UvRuntimeSource::PipRequirementsText {
                    contents: NonEmptyString::try_new("sglang==0.4.0\n".to_owned())?,
                    install_flags: None,
                },
            }),
        ];

        let variants = NonEmptyVec::try_new(vec![Variant {
            models: all_models,
            runtimes: NonEmptyVec::try_new(runtimes.to_vec())?,
            clients: NonEmptyVec::try_new(vec![ClientId::try_new("ev1_a".to_owned())?])?,
            benchmarks: None,
            runtime_flags: vec![],
            model_flags: vec![],
            benchmark_flags: vec![],
        }])?;

        let original = Matrix {
            benchmarks: Some(NonEmptyVec::try_new(vec![bench(
                "prefill_throughput_256",
            )?])?),
            variants,
        };

        let s = toml::to_string(&original)?;
        let round: Matrix = toml::from_str(&s)?;
        assert_eq!(original, round);
        Ok(())
    }

    #[test]
    fn parses_every_model_variant() -> anyhow::Result<()> {
        let toml_str = plan_toml(
            r#"models = [
  { type = "gguf_text", source = "huggingface", org = "meta-llama", repo_name = "llama-3.2-1b", path = "Q4_K_M.gguf" },
  { type = "gguf_vision", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-Vision-3B", model = "q4_K_M.gguf", mmproj = "mmproj-f16.gguf" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" },
  { type = "torch", source = "huggingface", org = "meta-llama", repo_name = "Llama-3.2-1B" },
  { type = "openvino", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-ov", prefix = "int4-sym-cw" },
  { type = "onnx_genai", source = "huggingface", org = "microsoft", repo_name = "Phi-3-mini-4k-instruct-onnx", prefix = "cpu_and_mobile/cpu-int4-rtn-block-32" },
]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b5000", flavor = "windows-x64-vulkan" }]
clients = ["ev1_c"]"#,
        );

        let req: Matrix = toml::from_str(&toml_str)?;
        let models = &req.variants[0].models;
        assert!(matches!(models[0], Model::GgufText(_)));
        assert!(matches!(models[1], Model::GgufVision(_)));
        assert!(matches!(models[2], Model::Mlx(_)));
        assert!(matches!(models[3], Model::Torch(_)));
        assert!(matches!(models[4], Model::Openvino(_)));
        assert!(matches!(models[5], Model::OnnxGenai(_)));
        Ok(())
    }

    #[test]
    fn apple_foundation_text_variant_round_trips() -> anyhow::Result<()> {
        let toml_str = plan_toml(
            r#"models = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]
clients = ["ev1_c"]"#,
        );

        let req: Matrix = toml::from_str(&toml_str)?;
        let model = &req.variants[0].models[0];
        let runtime = &req.variants[0].runtimes[0];
        assert!(matches!(model, Model::AppleFoundationText));
        assert!(matches!(runtime, Runtime::AppleFoundation(_)));

        assert_eq!(model.to_string(), "apple/foundation-text");
        assert_eq!(runtime.to_string(), "apple_foundation");

        assert!(model.auth_token().is_none());

        let emitted = toml::to_string(&req)?;
        let reparsed: Matrix = toml::from_str(&emitted)?;
        assert_eq!(req, reparsed);
        Ok(())
    }

    #[rstest::rstest]
    #[case::no_token_omits_from_wire("", false)]
    #[case::token_round_trips("\nauth_token = \"hf_xxx\"", true)]
    fn auth_token_wire_form_round_trips(
        #[case] attrs: &str,
        #[case] expect: bool,
    ) -> anyhow::Result<()> {
        let toml_str = format!(
            r#"type = "gguf_text"
source = "huggingface"
org = "meta-llama"
repo_name = "llama-3.2-1b"
path = "Q4_K_M.gguf"{attrs}"#
        );
        let model: Model = toml::from_str(&toml_str)?;
        let Model::GgufText(m) = &model else {
            return Err(anyhow::anyhow!("expected GgufText, got {model:?}"));
        };
        assert_eq!(m.source.auth_token().is_some(), expect, "parse");
        let emitted = toml::to_string(&model)?;
        assert_eq!(
            emitted.contains("auth_token"),
            expect,
            "wire form ({emitted})",
        );
        let reparsed: Model = toml::from_str(&emitted)?;
        let Model::GgufText(m) = &reparsed else {
            return Err(anyhow::anyhow!("reparsed wrong variant"));
        };
        assert_eq!(m.source.auth_token().is_some(), expect, "re-parse");
        Ok(())
    }

    // Duplicates part of `Model`'s doc example; update both together.
    #[test]
    fn doc_comment_model_example_parses() -> anyhow::Result<()> {
        let toml_str = plan_toml(
            r#"models = [
  { type = "gguf_text",   source = "huggingface", org = "meta-llama", repo_name = "llama-3.2-1b", path = "Q4_K_M.gguf" },
  { type = "gguf_vision", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-Vision-3B", model = "q4_K_M.gguf", mmproj = "mmproj-f16.gguf" },
  { type = "mlx",         source = "huggingface", org = "LiquidAI",   repo_name = "LFM2.5-350M-MLX-4bit" },
  { type = "torch",       source = "huggingface", org = "meta-llama", repo_name = "Llama-3.2-1B" },
]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b5000", flavor = "windows-x64-vulkan" }]
clients = ["ev1_c"]"#,
        );
        // Deserialize only: `Plan::parse` would reject this multi-kind example.
        toml::from_str::<Matrix>(&toml_str)?;
        Ok(())
    }

    /// Every vault arm round-trips through JSON; the flattened vision files are
    /// where a field clash would reshape the value.
    #[rstest::rstest]
    #[case::gguf_text(
        "type = \"gguf_text\"\nsource = \"vault_url\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\""
    )]
    #[case::gguf_vision(
        "type = \"gguf_vision\"\nsource = \"vault_url\"\nmodel = \"/vault/resources/vault_1/content\"\nmmproj = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\""
    )]
    #[case::gguf_vision_speculative(
        "type = \"gguf_vision_speculative\"\nsource = \"vault_url\"\nmodel = \"/vault/resources/vault_1/content\"\nmmproj = \"/vault/resources/vault_1/content\"\ndraft = \"/vault/resources/vault_1/content\"\nprincipal_id = \"user_01936d\""
    )]
    #[case::mlx(
        "type = \"mlx\"\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\""
    )]
    #[case::torch_pinned(
        "type = \"torch\"\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\"\nsha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\nprincipal_id = \"org_01936d\""
    )]
    #[case::mlx_speculative_pair(
        "type = \"mlx_speculative\"\n[target]\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\"\n[draft]\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\""
    )]
    fn vault_models_round_trip_through_json(#[case] toml_src: &str) -> anyhow::Result<()> {
        let model: Model = toml::from_str(toml_src).context("should parse")?;
        let back: Model = serde_json::from_str(&serde_json::to_string(&model)?)
            .context("should round-trip through JSON")?;
        assert_eq!(model, back);
        Ok(())
    }

    /// The label is trimmed, and one without a label writes none.
    #[rstest::rstest]
    #[case::gguf_text(
        "type = \"gguf_text\"\nsource = \"vault_url\"\nurl = \"/vault/resources/vault_1/content\""
    )]
    #[case::gguf_vision(
        "type = \"gguf_vision\"\nsource = \"vault_url\"\nmodel = \"/vault/resources/vault_1/content\"\nmmproj = \"/vault/resources/vault_2/content\""
    )]
    #[case::mlx(
        "type = \"mlx\"\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\""
    )]
    fn a_vault_model_keeps_its_custom_label(#[case] source: &str) -> anyhow::Result<()> {
        let plain = format!("{source}\nprincipal_id = \"org_01936d\"");
        let unlabelled = serde_json::to_value(toml::from_str::<Model>(&plain)?)?;
        assert_eq!(unlabelled.get("custom_label"), None, "{unlabelled}");

        let labelled: Model =
            toml::from_str(&format!("{plain}\ncustom_label = \" acme ACMELM q4 \""))?;
        let json = serde_json::to_value(&labelled)?;
        assert_eq!(
            json.get("custom_label"),
            Some(&serde_json::json!("acme ACMELM q4"))
        );
        assert_eq!(serde_json::from_value::<Model>(json)?, labelled);

        let blank = toml::from_str::<Model>(&format!("{plain}\ncustom_label = \"  \""));
        assert!(blank.is_err(), "a blank label says nothing: {blank:?}");
        Ok(())
    }

    /// Not an absolute URL, which could point the fetch at another host.
    #[rstest::rstest]
    #[case::gguf_text_local("gguf_text", "vault_url", "file:///models/a.gguf")]
    #[case::mlx_local("mlx", "vault_archive", "file:///models/a")]
    #[case::gguf_text_absolute(
        "gguf_text",
        "vault_url",
        "https://elsewhere.example/vault/resources/vault_1/content"
    )]
    fn a_vault_model_takes_only_the_route(
        #[case] model_type: &str,
        #[case] source: &str,
        #[case] url: &str,
    ) -> anyhow::Result<()> {
        let err = toml::from_str::<Model>(&format!(
            "type = \"{model_type}\"\nsource = \"{source}\"\nurl = \"{url}\"\nprincipal_id = \"org_01936d\""
        ))
        .err()
        .context("anything but the route should be refused")?;
        assert!(err.to_string().contains("vault path"), "got {err}");
        Ok(())
    }

    /// Per type, so a format with its own source enum would show.
    #[rstest::rstest]
    #[case::mlx("mlx")]
    #[case::torch("torch")]
    #[case::openvino("openvino")]
    #[case::onnx_genai("onnx_genai")]
    fn every_directory_model_reaches_the_vault(#[case] model_type: &str) -> anyhow::Result<()> {
        let model: Model = toml::from_str(&format!(
            "type = \"{model_type}\"\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\""
        ))
        .context("a vault archive should parse for every directory-shaped model")?;
        assert_eq!(model.to_string(), "/vault/resources/vault_1/content");
        Ok(())
    }

    /// The device signature is a vault arm's credential, not a host token.
    #[test]
    fn a_vault_source_takes_no_hf_token() -> anyhow::Result<()> {
        let mut model: Model = toml::from_str(
            "type = \"mlx\"\nsource = \"vault_archive\"\nurl = \"/vault/resources/vault_1/content\"\nprincipal_id = \"org_01936d\"",
        )?;
        assert!(!inject_hf_auth_token(
            &mut model,
            AuthToken::try_new("hf_x".to_owned())?
        ));
        Ok(())
    }
}
