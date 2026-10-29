//! `VlmEvalRecipe`: the models a `vlm_eval` or `asr_eval` catalog entry is the
//! recipe for. One type for both: an `asr_eval` recipe is a `vlm_eval` one whose
//! thinking mode is always off.
//!
//! The entry already fixes how a model is asked and decoded: the prompt
//! profile, the sampling knobs, the length budget. The recipe says which
//! models those settings belong to, the thinking mode a cell must run them in,
//! and the public pages each setting is taken from. A cell running a model the
//! entry does not cover, or in another thinking mode, is refused before it is
//! queued and again before it runs, so a score is always a vendor's recipe.

use serde::{Deserialize, Serialize};

use crate::{
    BenchmarkType, GgufVision, GgufVisionSource, HfOrg, HfRepo, HfRepoError, HfRepoName, Model,
    ModelSource, ModelType, NonEmptyTrimmedString, NonEmptyVec, RepoSubpath, ResourceUrl, Torch,
};

/// Who an entry's settings are for, and where they come from.
/// No `deny_unknown_fields`: a field a newer server adds must not make the
/// definition unreadable to a client. The server's catalog loader refuses an
/// unknown key here instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlmEvalRecipe {
    /// The model family, as the methodology's tables name it.
    pub family: NonEmptyTrimmedString,
    /// Every model the settings are for. Each line covers every gguf in its
    /// repo, so a quant grid needs no more lines than the family has sizes, or
    /// a repo's own (torch) weights.
    pub models: NonEmptyVec<VlmEvalRecipeModel>,
    /// The thinking mode every cell runs: a cell's `enable_thinking` model
    /// flag must say exactly this. Absent for a family whose chat template has
    /// no switch: it thinks through its prompt, or never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    /// The public pages the settings are taken from: the vendor's model card,
    /// its VLMEvalKit wrapper at a pinned commit. `https` only.
    pub sources: NonEmptyVec<ResourceUrl>,
    /// An asr_eval recipe's only: the languages the model's card declares for
    /// speech recognition, read from the card among `sources`. A cell on a
    /// task in another language runs and is scored, and a report marks it as
    /// outside the card's declared support
    /// ([`super::AsrEvalBenchmark::within_declared_languages`]). Absent when
    /// the card enumerates none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<NonEmptyVec<LanguageSubtag>>,
}

/// A language as a BCP 47 primary language subtag: the ISO 639-1 code where
/// one exists, else the ISO 639-3 code (`en`, `fr`, `yue`); two or three
/// lowercase ASCII letters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LanguageSubtag(String);

/// A language that is not two or three lowercase ASCII letters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a language subtag (two or three lowercase ASCII letters, e.g. `en`, `yue`)")]
pub struct LanguageSubtagError(String);

impl LanguageSubtag {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LanguageSubtag {
    type Error = LanguageSubtagError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if (2..=3).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(raw))
        } else {
            Err(LanguageSubtagError(raw))
        }
    }
}

impl From<LanguageSubtag> for String {
    fn from(subtag: LanguageSubtag) -> Self {
        subtag.0
    }
}

impl std::fmt::Display for LanguageSubtag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One model a recipe covers. Read leniently, as [`VlmEvalRecipe`] is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlmEvalRecipeModel {
    /// The Hugging Face repo the ggufs, or the torch weights, come from,
    /// `org/repo_name`.
    pub repo: RecipeRepo,
    /// The projector the recipe evaluates with, for a gguf repo that ships
    /// more than one; a cell must load exactly this file. Absent, any. A line
    /// naming one covers ggufs alone: a torch model loads its repo's own
    /// processor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj: Option<RepoSubpath>,
}

/// An `org/repo_name` Hugging Face repo, compared without regard to case, as
/// the Hub resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RecipeRepo {
    org: HfOrg,
    repo_name: HfRepoName,
}

impl RecipeRepo {
    /// Whether `repo` names this repo, at any revision.
    pub fn is(&self, repo: &HfRepo) -> bool {
        self.org.as_ref().eq_ignore_ascii_case(repo.org.as_ref())
            && self
                .repo_name
                .as_ref()
                .eq_ignore_ascii_case(repo.repo_name.as_ref())
    }

    /// Whether two lines name the same repo.
    pub fn same_as(&self, other: &RecipeRepo) -> bool {
        self.org.as_ref().eq_ignore_ascii_case(other.org.as_ref())
            && self
                .repo_name
                .as_ref()
                .eq_ignore_ascii_case(other.repo_name.as_ref())
    }
}

impl TryFrom<String> for RecipeRepo {
    type Error = HfRepoError;

    fn try_from(org_repo: String) -> Result<Self, Self::Error> {
        let repo = HfRepo::parse_org_repo(&org_repo)?;
        Ok(Self {
            org: repo.org,
            repo_name: repo.repo_name,
        })
    }
}

impl From<RecipeRepo> for String {
    fn from(repo: RecipeRepo) -> Self {
        repo.to_string()
    }
}

impl std::fmt::Display for RecipeRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.org, self.repo_name)
    }
}

/// A thinking mode as a message spells it: `true`, `false`, or `unset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thinking(pub Option<bool>);

impl std::fmt::Display for Thinking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(on) => write!(f, "{on}"),
            None => f.write_str("unset"),
        }
    }
}

/// Why a cell does not run its entry's recipe. Each names what the recipe
/// says, so the message is the fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VlmEvalRecipeError {
    /// Only a Hub repo says which model a file is.
    #[error(
        "the {family} recipe covers gguf_vision and torch models from Hugging Face repos; the \
         cell's {model_type} model is not one"
    )]
    NotAHubModel {
        family: String,
        model_type: ModelType,
    },
    #[error("the {family} recipe does not cover {repo} (it covers {covered})")]
    NotCovered {
        family: String,
        repo: String,
        covered: String,
    },
    #[error("the {family} recipe evaluates {repo} with projector {expected}; the cell loads {got}")]
    OtherProjector {
        family: String,
        repo: String,
        expected: String,
        got: String,
    },
    /// A recipe covers a repo's own weights, not another model packed in one
    /// of its directories.
    #[error(
        "the {family} recipe covers the weights at the root of {repo}; the cell loads its \
         `{prefix}` directory"
    )]
    Subdirectory {
        family: String,
        repo: String,
        prefix: String,
    },
    #[error(
        "the {family} recipe runs with enable_thinking = {expected}; the cell's vlm_eval model \
         flags say {got}"
    )]
    Thinking {
        family: String,
        expected: Thinking,
        got: Thinking,
    },
    /// [`Self::Thinking`] on a cell of another benchmark type than vlm_eval.
    #[error(
        "the {family} recipe runs with enable_thinking = {expected}; the cell's {benchmark} \
         model flags say {got}"
    )]
    CellThinking {
        benchmark: BenchmarkType,
        family: String,
        expected: Thinking,
        got: Thinking,
    },
}

impl VlmEvalRecipe {
    /// Whether a cell running `model` in thinking mode `enable_thinking` (its
    /// model flags', `None` for none stated) runs this recipe: a covered repo,
    /// the recipe's projector if it names one, and its thinking mode.
    pub fn check(
        &self,
        model: &Model,
        enable_thinking: Option<bool>,
    ) -> Result<(), VlmEvalRecipeError> {
        let family = || self.family.to_string();
        // The repo, the gguf projector a gguf model loads (a torch model's
        // processor is its repo's own), and the subdirectory a torch model
        // names, refused once the repo is known to be covered: an uncovered
        // repo's refusal is that it is not covered.
        let (repo, mmproj, prefix) = match model {
            Model::GgufVision(GgufVision {
                source: GgufVisionSource::HuggingFace(files),
            }) => (&files.repo, Some(&files.mmproj), None),
            Model::Torch(Torch {
                source: ModelSource::HuggingFace { repo, prefix },
            }) => (repo, None, prefix.as_ref()),
            // Another source (relative or absolute files, a URL, the vault)
            // names no repo; another model kind is not a vision model a
            // recipe can cover.
            Model::GgufVision(_)
            | Model::Torch(_)
            | Model::GgufText(_)
            | Model::GgufVisionSpeculative(_)
            | Model::Mlx(_)
            | Model::MlxSpeculative(_)
            | Model::TorchSpeculative(_)
            | Model::Openvino(_)
            | Model::OnnxGenai(_)
            | Model::AppleFoundationText => {
                return Err(VlmEvalRecipeError::NotAHubModel {
                    family: family(),
                    model_type: ModelType::of(model),
                });
            }
        };
        let line = self
            .models
            .iter()
            .find(|line| line.repo.is(repo))
            .ok_or_else(|| VlmEvalRecipeError::NotCovered {
                family: family(),
                repo: repo.to_string(),
                covered: self.covered(),
            })?;
        if let Some(prefix) = prefix {
            return Err(VlmEvalRecipeError::Subdirectory {
                family: family(),
                repo: repo.to_string(),
                prefix: prefix.to_string(),
            });
        }
        if let Some(expected) = &line.mmproj
            && mmproj != Some(expected)
        {
            return Err(VlmEvalRecipeError::OtherProjector {
                family: family(),
                repo: repo.to_string(),
                expected: expected.to_string(),
                got: mmproj.map_or_else(
                    || "the repo's own processor (a torch model)".to_owned(),
                    ToString::to_string,
                ),
            });
        }
        let got = enable_thinking;
        if got != self.enable_thinking {
            return Err(VlmEvalRecipeError::Thinking {
                family: family(),
                expected: Thinking(self.enable_thinking),
                got: Thinking(got),
            });
        }
        Ok(())
    }

    /// [`Self::check`] for a cell of `benchmark` (a vlm_eval's or an
    /// asr_eval's), whose refusal names that cell's model flags.
    pub fn check_cell(
        &self,
        benchmark: BenchmarkType,
        model: &Model,
        enable_thinking: Option<bool>,
    ) -> Result<(), VlmEvalRecipeError> {
        match self.check(model, enable_thinking) {
            Err(VlmEvalRecipeError::Thinking {
                family,
                expected,
                got,
            }) if benchmark != BenchmarkType::VlmEval => Err(VlmEvalRecipeError::CellThinking {
                benchmark,
                family,
                expected,
                got,
            }),
            checked => checked,
        }
    }

    /// Whether the card declares `language` for speech recognition: `None`
    /// when the recipe enumerates no languages.
    pub fn declares(&self, language: &str) -> Option<bool> {
        self.languages
            .as_ref()
            .map(|languages| languages.iter().any(|l| l.as_str() == language))
    }

    /// Whether the recipe covers `repo`, at any revision.
    pub fn covers(&self, repo: &HfRepo) -> bool {
        self.models.iter().any(|line| line.repo.is(repo))
    }

    /// Whether the recipe covers `repo`'s own (torch) weights, at any
    /// revision: a line for it that names no projector, as [`Self::check`]
    /// requires of a torch model.
    pub fn covers_own_weights(&self, repo: &HfRepo) -> bool {
        self.models
            .iter()
            .any(|line| line.repo.is(repo) && line.mmproj.is_none())
    }

    /// The repos a refusal names: the first few, and how many more, so a
    /// recipe of any size gives a message of bounded length.
    fn covered(&self) -> String {
        const NAMED: usize = 4;
        let named = self
            .models
            .iter()
            .take(NAMED)
            .map(|line| line.repo.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        match self.models.len().saturating_sub(NAMED) {
            0 => named,
            more => format!("{named} and {more} more"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn recipe(thinking: Option<bool>, mmproj: Option<&str>) -> anyhow::Result<VlmEvalRecipe> {
        let mut line = json!({"repo": "unsloth/Qwen3.5-2B-GGUF"});
        if let Some(mmproj) = mmproj {
            line["mmproj"] = json!(mmproj);
        }
        let mut recipe = json!({
            "family": "Qwen3.5",
            "models": [line, {"repo": "unsloth/Qwen3.5-4B-GGUF"}],
            "sources": ["https://huggingface.co/Qwen/Qwen3.5-2B"],
        });
        if let Some(thinking) = thinking {
            recipe["enable_thinking"] = json!(thinking);
        }
        Ok(serde_json::from_value(recipe)?)
    }

    fn model(org: &str, repo: &str, mmproj: &str) -> anyhow::Result<Model> {
        Ok(serde_json::from_value(json!({
            "type": "gguf_vision", "source": "huggingface", "org": org, "repo_name": repo,
            "revision": "abc", "model": "Qwen3.5-2B-Q4_K_M.gguf", "mmproj": mmproj,
        }))?)
    }

    /// Any quant of a covered repo, at any revision and in any case, with the
    /// recipe's thinking mode, runs the recipe.
    #[rstest]
    #[case::listed("unsloth", "Qwen3.5-2B-GGUF")]
    #[case::other_case("Unsloth", "qwen3.5-2b-gguf")]
    #[case::second_line("unsloth", "Qwen3.5-4B-GGUF")]
    fn a_covered_repo_in_the_recipes_mode_runs(
        #[case] org: &str,
        #[case] repo: &str,
    ) -> anyhow::Result<()> {
        let recipe = recipe(Some(true), None)?;
        recipe.check(&model(org, repo, "mmproj-BF16.gguf")?, Some(true))?;
        Ok(())
    }

    /// A client on an older pin still reads a recipe a newer server added a
    /// field to, at either level.
    #[test]
    fn a_recipe_with_a_newer_field_still_reads() -> anyhow::Result<()> {
        serde_json::from_value::<VlmEvalRecipe>(json!({
            "family": "Qwen3.5",
            "models": [{"repo": "unsloth/Qwen3.5-2B-GGUF", "later": 1}],
            "sources": ["https://huggingface.co/Qwen/Qwen3.5-2B"],
            "later": true,
        }))?;
        Ok(())
    }

    #[test]
    fn another_repo_is_not_covered() -> anyhow::Result<()> {
        let err = recipe(Some(true), None)?
            .check(
                &model("bartowski", "Qwen3.5-2B-GGUF", "mmproj.gguf")?,
                Some(true),
            )
            .err();
        let Some(VlmEvalRecipeError::NotCovered { repo, covered, .. }) = err else {
            anyhow::bail!("expected NotCovered, got {err:?}");
        };
        assert_eq!(repo, "bartowski/Qwen3.5-2B-GGUF");
        assert_eq!(covered, "unsloth/Qwen3.5-2B-GGUF, unsloth/Qwen3.5-4B-GGUF");
        Ok(())
    }

    /// A recipe's languages are BCP 47 primary subtags; anything else does
    /// not parse.
    #[rstest]
    #[case::alpha_2("en", true)]
    #[case::alpha_3("yue", true)]
    #[case::upper("EN", false)]
    #[case::one_letter("e", false)]
    #[case::a_name("english", false)]
    #[case::a_region("en-US", false)]
    #[case::empty("", false)]
    fn a_language_is_a_primary_subtag(#[case] raw: &str, #[case] parses: bool) {
        let parsed: Result<VlmEvalRecipe, _> = serde_json::from_value(json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"],
            "languages": [raw],
        }));
        assert_eq!(parsed.is_ok(), parses, "{raw:?}: {parsed:?}");
    }

    /// A recipe says whether its card declares a language, and nothing when it
    /// lists none (an empty list does not parse).
    #[test]
    fn a_recipe_declares_its_cards_languages() -> anyhow::Result<()> {
        let listed: VlmEvalRecipe = serde_json::from_value(json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"],
            "languages": ["en", "zh"],
        }))?;
        assert_eq!(listed.declares("en"), Some(true));
        assert_eq!(listed.declares("de"), Some(false));
        assert_eq!(recipe(Some(false), None)?.declares("en"), None);
        let empty: Result<VlmEvalRecipe, _> = serde_json::from_value(json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"], "languages": [],
        }));
        assert!(empty.is_err(), "{empty:?}");
        assert!(
            !serde_json::to_string(&recipe(Some(false), None)?)?.contains("languages"),
            "an absent list is not written"
        );
        Ok(())
    }

    /// A long recipe is named by its first few repos.
    #[test]
    fn a_refusal_names_a_long_recipe_briefly() -> anyhow::Result<()> {
        let models: Vec<_> = (0..40)
            .map(|i| json!({"repo": format!("o/r{i}")}))
            .collect();
        let recipe: VlmEvalRecipe = serde_json::from_value(json!({
            "family": "F", "models": models, "sources": ["https://a"],
        }))?;
        let err = recipe
            .check(&model("x", "y", "p.gguf")?, None)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.ends_with("(it covers o/r0, o/r1, o/r2, o/r3 and 36 more)"),
            "{err}"
        );
        Ok(())
    }

    /// The mode is compared whole: unset is not "the recipe's default".
    #[rstest]
    #[case::off_for_on(Some(true), Some(false))]
    #[case::unset_for_on(Some(true), None)]
    #[case::on_for_unset(None, Some(true))]
    #[case::off_for_unset(None, Some(false))]
    fn another_thinking_mode_is_refused(
        #[case] recipe_mode: Option<bool>,
        #[case] cell_mode: Option<bool>,
    ) -> anyhow::Result<()> {
        let err = recipe(recipe_mode, None)?
            .check(
                &model("unsloth", "Qwen3.5-2B-GGUF", "mmproj-BF16.gguf")?,
                cell_mode,
            )
            .err();
        assert_eq!(
            err,
            Some(VlmEvalRecipeError::Thinking {
                family: "Qwen3.5".into(),
                expected: Thinking(recipe_mode),
                got: Thinking(cell_mode),
            })
        );
        Ok(())
    }

    /// A family with no switch runs with no flags at all.
    #[test]
    fn no_flags_match_an_unset_mode() -> anyhow::Result<()> {
        recipe(None, None)?.check(
            &model("unsloth", "Qwen3.5-2B-GGUF", "mmproj-BF16.gguf")?,
            None,
        )?;
        let err = recipe(Some(true), None)?
            .check(
                &model("unsloth", "Qwen3.5-2B-GGUF", "mmproj-BF16.gguf")?,
                None,
            )
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(
            err,
            "the Qwen3.5 recipe runs with enable_thinking = true; the cell's vlm_eval model flags say unset"
        );
        Ok(())
    }

    /// An asr_eval cell's refusal names its own model flags; a vlm_eval cell's
    /// reads as [`VlmEvalRecipe::check`]'s.
    #[test]
    fn a_cells_refusal_names_its_benchmark() -> anyhow::Result<()> {
        let recipe = recipe(Some(false), None)?;
        let model = model("unsloth", "Qwen3.5-2B-GGUF", "mmproj-BF16.gguf")?;
        let refusal = |benchmark| {
            recipe
                .check_cell(benchmark, &model, None)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert_eq!(
            refusal(BenchmarkType::AsrEval),
            "the Qwen3.5 recipe runs with enable_thinking = false; the cell's asr_eval model flags say unset"
        );
        assert_eq!(
            refusal(BenchmarkType::VlmEval),
            "the Qwen3.5 recipe runs with enable_thinking = false; the cell's vlm_eval model flags say unset"
        );
        recipe.check_cell(BenchmarkType::AsrEval, &model, Some(false))?;
        Ok(())
    }

    #[test]
    fn a_pinned_projector_must_be_the_one_loaded() -> anyhow::Result<()> {
        let recipe = recipe(None, Some("mmproj-model-f16-4x.gguf"))?;
        recipe.check(
            &model("unsloth", "Qwen3.5-2B-GGUF", "mmproj-model-f16-4x.gguf")?,
            None,
        )?;
        let err = recipe
            .check(
                &model("unsloth", "Qwen3.5-2B-GGUF", "mmproj-model-f16.gguf")?,
                None,
            )
            .err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::OtherProjector { ref expected, ref got, .. })
                if expected == "mmproj-model-f16-4x.gguf" && got == "mmproj-model-f16.gguf"),
            "{err:?}"
        );
        // The pin is per line: the 4B line names none, so any projector runs.
        recipe.check(
            &model("unsloth", "Qwen3.5-4B-GGUF", "mmproj-model-f16.gguf")?,
            None,
        )?;
        Ok(())
    }

    /// A model no repo names cannot be matched to a line.
    #[test]
    fn a_model_off_the_hub_is_refused() -> anyhow::Result<()> {
        let local: Model = serde_json::from_value(json!({
            "type": "gguf_vision", "source": "relative_files",
            "model": "m.gguf", "mmproj": "p.gguf",
        }))?;
        let err = recipe(None, None)?.check(&local, None).err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::NotAHubModel { .. })),
            "{err:?}"
        );
        Ok(())
    }

    fn torch(org: &str, repo: &str, prefix: Option<&str>) -> anyhow::Result<Model> {
        let mut model = json!({
            "type": "torch", "source": "huggingface", "org": org, "repo_name": repo,
            "revision": "abc",
        });
        if let Some(prefix) = prefix {
            model["prefix"] = json!(prefix);
        }
        Ok(serde_json::from_value(model)?)
    }

    /// A repo's own weights run the recipe that names the repo, in its mode, as
    /// a gguf of a covered repo does.
    #[rstest]
    #[case::listed("Qwen", "Qwen3.5-2B")]
    #[case::other_case("qwen", "QWEN3.5-2B")]
    fn a_covered_torch_repo_in_the_recipes_mode_runs(
        #[case] org: &str,
        #[case] repo: &str,
    ) -> anyhow::Result<()> {
        let recipe: VlmEvalRecipe = serde_json::from_value(json!({
            "family": "Qwen3.5",
            "models": [{"repo": "unsloth/Qwen3.5-2B-GGUF"}, {"repo": "Qwen/Qwen3.5-2B"}],
            "enable_thinking": true,
            "sources": ["https://huggingface.co/Qwen/Qwen3.5-2B"],
        }))?;
        recipe.check(&torch(org, repo, None)?, Some(true))?;
        let err = recipe.check(&torch(org, repo, None)?, Some(false)).err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::Thinking { .. })),
            "{err:?}"
        );
        Ok(())
    }

    /// The gguf lines do not cover the original repo, nor a torch model a gguf
    /// repo, which holds no torch weights.
    #[test]
    fn a_torch_model_needs_its_own_line() -> anyhow::Result<()> {
        let recipe = recipe(Some(true), None)?;
        let err = recipe
            .check(&torch("Qwen", "Qwen3.5-2B", None)?, Some(true))
            .err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::NotCovered { ref repo, .. }) if repo == "Qwen/Qwen3.5-2B"),
            "{err:?}"
        );
        Ok(())
    }

    /// A model packed in one of a covered repo's directories is another model
    /// than the repo's own.
    #[test]
    fn a_torch_subdirectory_is_refused() -> anyhow::Result<()> {
        let recipe: VlmEvalRecipe = serde_json::from_value(json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"],
        }))?;
        let err = recipe.check(&torch("o", "r", Some("int4"))?, None).err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::Subdirectory { ref prefix, .. }) if prefix == "int4"),
            "{err:?}"
        );
        Ok(())
    }

    /// A subdirectory of a repo the recipe does not cover is refused as the
    /// repo it is: not covered, not a subdirectory to drop.
    #[test]
    fn an_uncovered_torch_subdirectory_is_not_covered() -> anyhow::Result<()> {
        let recipe: VlmEvalRecipe = serde_json::from_value(json!({
            "family": "F", "models": [{"repo": "o/r"}], "sources": ["https://a"],
        }))?;
        let err = recipe
            .check(&torch("o", "other", Some("int4"))?, None)
            .err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::NotCovered { ref repo, .. }) if repo == "o/other"),
            "{err:?}"
        );
        Ok(())
    }

    /// A line that pins a gguf projector covers the ggufs alone: a torch model
    /// loads its repo's own processor, never that file.
    #[test]
    fn a_projector_line_does_not_cover_a_torch_model() -> anyhow::Result<()> {
        let recipe: VlmEvalRecipe = serde_json::from_value(json!({
            "family": "F", "models": [{"repo": "o/r", "mmproj": "mmproj-4x.gguf"}],
            "sources": ["https://a"],
        }))?;
        let err = recipe.check(&torch("o", "r", None)?, None).err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::OtherProjector { ref expected, .. }) if expected == "mmproj-4x.gguf"),
            "{err:?}"
        );
        Ok(())
    }

    /// Torch weights from the vault name no repo, as gguf files off the Hub do.
    #[test]
    fn a_torch_model_off_the_hub_is_refused() -> anyhow::Result<()> {
        let vaulted: Model = serde_json::from_value(json!({
            "type": "torch", "source": "vault_archive",
            "url": "/vault/resources/vault_01a0aa86-d09a-77a3-a888-cdfb0814c3dc/content",
            "principal_id": "org_01a0aa86-d09a-77a3-a888-cdfb0814c3dc",
        }))?;
        let err = recipe(None, None)?.check(&vaulted, None).err();
        assert!(
            matches!(err, Some(VlmEvalRecipeError::NotAHubModel { .. })),
            "{err:?}"
        );
        Ok(())
    }

    #[rstest]
    #[case::no_models(json!({"family": "F", "models": [], "sources": ["https://a"]}))]
    #[case::no_sources(json!({"family": "F", "models": [{"repo": "o/r"}], "sources": []}))]
    #[case::bare_repo(json!({"family": "F", "models": [{"repo": "r"}], "sources": ["https://a"]}))]
    #[case::blank_family(json!({"family": " ", "models": [{"repo": "o/r"}], "sources": ["https://a"]}))]
    fn a_malformed_recipe_does_not_parse(#[case] body: serde_json::Value) {
        assert!(serde_json::from_value::<VlmEvalRecipe>(body).is_err());
    }

    #[test]
    fn a_recipe_round_trips_through_toml() -> anyhow::Result<()> {
        let text = r#"
family = "MiniCPM-V 4.7"
models = [{ repo = "openbmb/MiniCPM-V-4.7-1B-gguf", mmproj = "mmproj-model-f16-4x.gguf" }]
enable_thinking = true
sources = ["https://huggingface.co/openbmb/MiniCPM-V-4.7-1B"]
"#;
        let recipe: VlmEvalRecipe = toml::from_str(text)?;
        assert_eq!(recipe.enable_thinking, Some(true));
        let again: VlmEvalRecipe = toml::from_str(&toml::to_string(&recipe)?)?;
        assert_eq!(again, recipe);
        Ok(())
    }
}
