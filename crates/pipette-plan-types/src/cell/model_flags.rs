//! Per-cell model-generation flags, keyed on `(benchmark, model)`.

use serde::{Deserialize, Serialize};

use crate::{BenchmarkType, Model, ModelType};

/// Why a [`ModelFlagRef`] did not resolve into a [`ModelFlags`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelFlagsError {
    /// Model flags apply only to the eval, vlm_eval and asr_eval cells, so any other pair
    /// is refused.
    #[error("no model flags defined for {model:?} on {benchmark:?}")]
    NoSuchCombination {
        benchmark: BenchmarkType,
        model: ModelType,
    },
    /// A knob was set on a cell whose variant has no field for it.
    #[error("{knob} is not a valid model flag for {model:?} on {benchmark:?}")]
    KnobNotAllowed {
        knob: &'static str,
        benchmark: BenchmarkType,
        model: ModelType,
    },
}

/// How much a reasoning model is asked to think: the `reasoning_effort` its chat
/// template reads. No `none`: vLLM refuses it and llama-server turns thinking
/// off instead, so the one word would name two different runs.
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
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// Why recorded [`ModelFlags`] did not read back against their cell.
pub type ModelFlagsRecordError = super::FlagsRecordError<ModelFlagsError>;

/// Per-cell model-generation flags, one arm per `(benchmark, model)` cell that
/// takes them: the eval cells and the vlm_eval and asr_eval cells, and a knob the cell has no
/// field for is refused. They attach to the cell, not to the [`Model`].
///
/// `enable_thinking` is how every eval says whether a reasoning model thinks,
/// text or vision: the plan declares it here, the run reports it as the
/// `model_flags` column (`enable_thinking=true`), the server stamps it as
/// `requested_model_flags`, and the scorer reads it from there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, strum::EnumCount)]
#[cfg_attr(test, derive(strum::IntoStaticStr))]
// As on `RuntimeFlags`, with a `(benchmark, model)` pair for the axes.
#[cfg_attr(
    feature = "schemars",
    derive(schemars::JsonSchema),
    schemars(!try_from, !into, tag = "variant", transform = crate::json_schema::pair_arms)
)]
#[serde(try_from = "ModelFlagRef", into = "ModelFlagRef")]
pub enum ModelFlags {
    // `reasoning_effort` only where the runtime forwards it to the model.
    EvalGgufText {
        enable_thinking: Option<bool>,
        reasoning_effort: Option<ReasoningEffort>,
    },
    EvalGgufVision {
        enable_thinking: Option<bool>,
    },
    EvalMlx {
        enable_thinking: Option<bool>,
    },
    EvalTorch {
        enable_thinking: Option<bool>,
        reasoning_effort: Option<ReasoningEffort>,
    },
    // The client applies it through the model's chat template and llama-server's
    // `--reasoning`, and requires it on a template that has a thinking switch.
    VlmEvalGgufVision {
        enable_thinking: Option<bool>,
    },
    // The client sends it as the request's `chat_template_kwargs`, and requires
    // it on a template that has a thinking switch.
    VlmEvalTorch {
        enable_thinking: Option<bool>,
    },
    // Always `Some(false)` on a cell its entry's recipe admits: every asr_eval
    // recipe runs with thinking off, and the cell states it as a vlm_eval
    // states its mode.
    AsrEvalGgufVision {
        enable_thinking: Option<bool>,
    },
    // As `AsrEvalGgufVision`, on a repo's own weights (vLLM).
    AsrEvalTorch {
        enable_thinking: Option<bool>,
    },
}

impl ModelFlags {
    /// The `(benchmark, model)` cell this variant encodes.
    pub fn axes(&self) -> (BenchmarkType, ModelType) {
        match self {
            ModelFlags::EvalGgufText { .. } => (BenchmarkType::Eval, ModelType::GgufText),
            ModelFlags::EvalGgufVision { .. } => (BenchmarkType::Eval, ModelType::GgufVision),
            ModelFlags::EvalMlx { .. } => (BenchmarkType::Eval, ModelType::Mlx),
            ModelFlags::EvalTorch { .. } => (BenchmarkType::Eval, ModelType::Torch),
            ModelFlags::VlmEvalGgufVision { .. } => (BenchmarkType::VlmEval, ModelType::GgufVision),
            ModelFlags::VlmEvalTorch { .. } => (BenchmarkType::VlmEval, ModelType::Torch),
            ModelFlags::AsrEvalGgufVision { .. } => (BenchmarkType::AsrEval, ModelType::GgufVision),
            ModelFlags::AsrEvalTorch { .. } => (BenchmarkType::AsrEval, ModelType::Torch),
        }
    }

    /// Whether this entry applies to a cell running `benchmark` on `model`.
    pub fn matches(&self, benchmark: BenchmarkType, model: &Model) -> bool {
        self.axes() == (benchmark, ModelType::of(model))
    }

    /// The `enable_thinking` chat-template kwarg override. On an eval `None` is
    /// not "no thinking": the kwarg is omitted and the engine's default applies
    /// (thinking on for llama-server and mlx_lm today). A vlm_eval refuses `None`
    /// on a template with a thinking switch, so its mode is always stated.
    pub fn enable_thinking(&self) -> Option<bool> {
        match self {
            ModelFlags::EvalGgufText {
                enable_thinking, ..
            }
            | ModelFlags::EvalGgufVision { enable_thinking }
            | ModelFlags::VlmEvalGgufVision { enable_thinking }
            | ModelFlags::VlmEvalTorch { enable_thinking }
            | ModelFlags::AsrEvalGgufVision { enable_thinking }
            | ModelFlags::AsrEvalTorch { enable_thinking }
            | ModelFlags::EvalMlx { enable_thinking }
            | ModelFlags::EvalTorch {
                enable_thinking, ..
            } => *enable_thinking,
        }
    }

    /// The effort a reasoning model is asked for. `None` omits it, and the
    /// template's default applies.
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        match self {
            ModelFlags::EvalGgufText {
                reasoning_effort, ..
            }
            | ModelFlags::EvalTorch {
                reasoning_effort, ..
            } => *reasoning_effort,
            ModelFlags::EvalGgufVision { .. }
            | ModelFlags::EvalMlx { .. }
            | ModelFlags::VlmEvalGgufVision { .. }
            | ModelFlags::VlmEvalTorch { .. }
            | ModelFlags::AsrEvalGgufVision { .. }
            | ModelFlags::AsrEvalTorch { .. } => None,
        }
    }

    /// Each set knob as `<knob>=<value>`, joined by `,` in a fixed order. Stored
    /// rows hash this spelling, so it must not change.
    pub fn canonical_string(&self) -> Option<String> {
        let knobs: Vec<String> = [
            self.enable_thinking()
                .map(|v| format!("enable_thinking={v}")),
            self.reasoning_effort()
                .map(|e| format!("reasoning_effort={e}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!knobs.is_empty()).then(|| knobs.join(","))
    }

    /// Recorded flags, an object of knobs, read back against their cell. An
    /// empty object is `None`.
    pub fn from_cell_json(
        json: &str,
        model_type: ModelType,
        benchmark_type: BenchmarkType,
    ) -> Result<Option<Self>, ModelFlagsRecordError> {
        let axes = [
            ("model_type", serde_json::to_value(model_type)?),
            ("benchmark_type", serde_json::to_value(benchmark_type)?),
        ];
        super::record::with_cell::<ModelFlagRef, _>(json, &axes)?
            .map(Self::try_from)
            .transpose()
            .map_err(ModelFlagsRecordError::Flags)
    }

    /// [`Self::canonical_string`] read back for a cell. Each knob may appear
    /// once, in either order; anything else reads as `None`.
    pub fn from_canonical_string(
        text: &str,
        benchmark: BenchmarkType,
        model: ModelType,
    ) -> Option<Self> {
        let mut flags = ModelFlagRef {
            model_type: model,
            benchmark_type: benchmark,
            enable_thinking: None,
            reasoning_effort: None,
        };
        for knob in text.split(',') {
            match knob.trim().split_once('=')? {
                ("enable_thinking", v) if flags.enable_thinking.is_none() => {
                    flags.enable_thinking = Some(v.parse().ok()?);
                }
                ("reasoning_effort", v) if flags.reasoning_effort.is_none() => {
                    flags.reasoning_effort = Some(v.parse().ok()?);
                }
                _ => return None,
            }
        }
        ModelFlags::try_from(flags).ok()
    }

    /// The submitted `model_flags`: `None` off the eval, vlm_eval and asr_eval cells, where
    /// the knobs have no effect and would only split warehouse joins. A new knob
    /// that affects throughput must be reported here; no compile error will say
    /// so.
    pub fn submission_string(&self, benchmark_type: BenchmarkType) -> Option<String> {
        match benchmark_type {
            BenchmarkType::Eval | BenchmarkType::VlmEval | BenchmarkType::AsrEval => {
                self.canonical_string()
            }
            BenchmarkType::PrefillThroughput
            | BenchmarkType::DecodeThroughput
            | BenchmarkType::EndToEndLatency
            | BenchmarkType::MaxMemoryUsage
            | BenchmarkType::VlThroughput
            | BenchmarkType::VlMaxMemory
            | BenchmarkType::VlSpecDecode
            | BenchmarkType::AsrThroughput
            | BenchmarkType::AsrMaxMemory => None,
        }
    }
}

/// Flat wire form of [`ModelFlags`]: the two axis keys plus the knobs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(all(test, feature = "schemars"), derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelFlagRef {
    pub model_type: ModelType,
    pub benchmark_type: BenchmarkType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl ModelFlagRef {
    /// Reject a `reasoning_effort` on a cell whose variant has no field for it.
    fn deny_reasoning_effort(&self) -> Result<(), ModelFlagsError> {
        match self.reasoning_effort {
            Some(_) => Err(ModelFlagsError::KnobNotAllowed {
                knob: "reasoning_effort",
                benchmark: self.benchmark_type,
                model: self.model_type,
            }),
            None => Ok(()),
        }
    }
}

impl TryFrom<ModelFlagRef> for ModelFlags {
    type Error = ModelFlagsError;

    fn try_from(r: ModelFlagRef) -> Result<Self, Self::Error> {
        let enable_thinking = r.enable_thinking;
        let reasoning_effort = r.reasoning_effort;
        Ok(match (r.benchmark_type, r.model_type) {
            (BenchmarkType::Eval, ModelType::GgufText) => ModelFlags::EvalGgufText {
                enable_thinking,
                reasoning_effort,
            },
            (BenchmarkType::Eval, ModelType::GgufVision) => {
                r.deny_reasoning_effort()?;
                ModelFlags::EvalGgufVision { enable_thinking }
            }
            (BenchmarkType::Eval, ModelType::Mlx) => {
                r.deny_reasoning_effort()?;
                ModelFlags::EvalMlx { enable_thinking }
            }
            (BenchmarkType::Eval, ModelType::Torch) => ModelFlags::EvalTorch {
                enable_thinking,
                reasoning_effort,
            },
            (BenchmarkType::VlmEval, ModelType::GgufVision) => {
                r.deny_reasoning_effort()?;
                ModelFlags::VlmEvalGgufVision { enable_thinking }
            }
            (BenchmarkType::VlmEval, ModelType::Torch) => {
                r.deny_reasoning_effort()?;
                ModelFlags::VlmEvalTorch { enable_thinking }
            }
            (BenchmarkType::AsrEval, ModelType::GgufVision) => {
                r.deny_reasoning_effort()?;
                ModelFlags::AsrEvalGgufVision { enable_thinking }
            }
            (BenchmarkType::AsrEval, ModelType::Torch) => {
                r.deny_reasoning_effort()?;
                ModelFlags::AsrEvalTorch { enable_thinking }
            }
            (benchmark, model) => {
                return Err(ModelFlagsError::NoSuchCombination { benchmark, model });
            }
        })
    }
}

impl From<ModelFlags> for ModelFlagRef {
    fn from(flags: ModelFlags) -> Self {
        let (benchmark_type, model_type) = flags.axes();
        ModelFlagRef {
            model_type,
            benchmark_type,
            enable_thinking: flags.enable_thinking(),
            reasoning_effort: flags.reasoning_effort(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[rstest::rstest]
    #[case::gguf_text(ModelType::GgufText, |enable_thinking| ModelFlags::EvalGgufText { enable_thinking, reasoning_effort: None })]
    #[case::gguf_vision(ModelType::GgufVision, |enable_thinking| ModelFlags::EvalGgufVision { enable_thinking })]
    #[case::mlx(ModelType::Mlx, |enable_thinking| ModelFlags::EvalMlx { enable_thinking })]
    #[case::torch(ModelType::Torch, |enable_thinking| ModelFlags::EvalTorch { enable_thinking, reasoning_effort: None })]
    fn model_flag_ref_routes_eval_pair_to_its_variant(
        #[case] model_type: ModelType,
        #[case] variant: fn(Option<bool>) -> ModelFlags,
        #[values(Some(true), Some(false), None)] enable_thinking: Option<bool>,
    ) -> anyhow::Result<()> {
        let flags = ModelFlags::try_from(ModelFlagRef {
            model_type,
            benchmark_type: BenchmarkType::Eval,
            enable_thinking,
            reasoning_effort: None,
        })?;
        assert_eq!(flags, variant(enable_thinking));
        assert_eq!(flags.axes(), (BenchmarkType::Eval, model_type));
        assert_eq!(flags.enable_thinking(), enable_thinking);
        assert_eq!(flags.reasoning_effort(), None);
        Ok(())
    }

    /// The hand-written case list above is tied to the enum only by this count.
    #[test]
    fn routing_cases_cover_every_variant() -> anyhow::Result<()> {
        use strum::EnumCount as _;

        let mut seen = std::collections::HashSet::new();
        for (benchmark_type, model_type) in [
            (BenchmarkType::Eval, ModelType::GgufText),
            (BenchmarkType::Eval, ModelType::GgufVision),
            (BenchmarkType::Eval, ModelType::Mlx),
            (BenchmarkType::Eval, ModelType::Torch),
            (BenchmarkType::VlmEval, ModelType::GgufVision),
            (BenchmarkType::VlmEval, ModelType::Torch),
            (BenchmarkType::AsrEval, ModelType::GgufVision),
            (BenchmarkType::AsrEval, ModelType::Torch),
        ] {
            let flags = ModelFlags::try_from(ModelFlagRef {
                model_type,
                benchmark_type,
                enable_thinking: Some(true),
                reasoning_effort: None,
            })?;
            anyhow::ensure!(
                seen.insert(std::mem::discriminant(&flags)),
                "{model_type:?} duplicates an earlier case's variant"
            );
        }
        anyhow::ensure!(
            seen.len() == ModelFlags::COUNT,
            "cases cover {} variants, enum has {}",
            seen.len(),
            ModelFlags::COUNT
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::gguf_text(ModelType::GgufText, true)]
    #[case::gguf_vision(ModelType::GgufVision, false)]
    #[case::mlx(ModelType::Mlx, false)]
    #[case::torch(ModelType::Torch, true)]
    fn reasoning_effort_routes_only_to_the_cells_that_take_it(
        #[case] model_type: ModelType,
        #[case] takes_it: bool,
        #[values(ReasoningEffort::Low, ReasoningEffort::Medium, ReasoningEffort::High)]
        effort: ReasoningEffort,
    ) {
        let flags = ModelFlags::try_from(ModelFlagRef {
            model_type,
            benchmark_type: BenchmarkType::Eval,
            enable_thinking: Some(true),
            reasoning_effort: Some(effort),
        });
        let expected = if takes_it {
            Ok((Some(true), Some(effort)))
        } else {
            Err(ModelFlagsError::KnobNotAllowed {
                knob: "reasoning_effort",
                benchmark: BenchmarkType::Eval,
                model: model_type,
            })
        };
        assert_eq!(
            flags.map(|f| (f.enable_thinking(), f.reasoning_effort())),
            expected
        );
    }

    #[rstest::rstest]
    #[case(ReasoningEffort::Low, "low")]
    #[case(ReasoningEffort::Medium, "medium")]
    #[case(ReasoningEffort::High, "high")]
    fn reasoning_effort_has_one_spelling(
        #[case] effort: ReasoningEffort,
        #[case] word: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(serde_json::to_value(effort)?, serde_json::json!(word));
        assert_eq!(effort.to_string(), word);
        assert_eq!(word.parse::<ReasoningEffort>()?, effort);
        Ok(())
    }

    #[cfg(feature = "schemars")]
    #[test]
    fn the_json_schema_has_an_arm_per_cell_with_its_knobs() -> anyhow::Result<()> {
        let table: std::collections::BTreeMap<Vec<String>, std::collections::BTreeSet<String>> = [
            (
                "eval",
                "gguf_text",
                &["enable_thinking", "reasoning_effort"][..],
            ),
            ("eval", "gguf_vision", &["enable_thinking"]),
            ("eval", "mlx", &["enable_thinking"]),
            ("eval", "torch", &["enable_thinking", "reasoning_effort"]),
            ("vlm_eval", "gguf_vision", &["enable_thinking"]),
            ("vlm_eval", "torch", &["enable_thinking"]),
            ("asr_eval", "gguf_vision", &["enable_thinking"]),
            ("asr_eval", "torch", &["enable_thinking"]),
        ]
        .into_iter()
        .map(|(benchmark, model, knobs)| {
            (
                vec![benchmark.to_owned(), model.to_owned()],
                knobs.iter().map(|knob| (*knob).to_owned()).collect(),
            )
        })
        .collect();
        let arms = crate::json_schema::flag_arms::<ModelFlags>(crate::json_schema::PAIR)?;
        assert_eq!(arms, table);
        Ok(())
    }

    #[cfg(feature = "schemars")]
    #[test]
    fn the_json_schema_has_an_arm_per_accepted_cell_with_its_knobs() -> anyhow::Result<()> {
        use strum::IntoEnumIterator as _;

        let pairs = BenchmarkType::ALL
            .iter()
            .flat_map(|b| ModelType::iter().map(move |m| vec![b.to_string(), m.to_string()]));
        crate::json_schema::assert_arms_are_accepted::<ModelFlags, ModelFlagRef>(
            crate::json_schema::PAIR,
            pairs,
            &[("enable_thinking", "true"), ("reasoning_effort", "\"low\"")],
        )
    }

    /// Walks every pair the axis types allow, so no variant escapes.
    #[test]
    fn variants_are_named_for_their_cell() -> anyhow::Result<()> {
        use strum::{EnumCount as _, IntoEnumIterator as _};

        let mut routed = std::collections::HashSet::new();
        let mut misnamed = Vec::new();
        for &b in BenchmarkType::ALL {
            for m in ModelType::iter() {
                let Ok(flags) = ModelFlags::try_from(ModelFlagRef {
                    model_type: m,
                    benchmark_type: b,
                    enable_thinking: None,
                    reasoning_effort: None,
                }) else {
                    continue;
                };
                anyhow::ensure!(
                    flags.axes() == (b, m),
                    "{b}/{m} routes to a variant whose axes() are {:?}",
                    flags.axes()
                );
                routed.insert(std::mem::discriminant(&flags));
                let name: &str = (&flags).into();
                let expected = format!("{b:?}{m:?}");
                if name != expected {
                    misnamed.push(format!("{name} (want {expected})"));
                }
            }
        }
        anyhow::ensure!(
            routed.len() == ModelFlags::COUNT,
            "pairs reach {} variants, the enum has {}",
            routed.len(),
            ModelFlags::COUNT
        );
        anyhow::ensure!(
            misnamed.is_empty(),
            "misnamed variants: {}",
            misnamed.join(", ")
        );
        Ok(())
    }

    const GGUF_TEXT: &str = "type = \"gguf_text\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\npath = \"m.gguf\"";
    const GGUF_VISION: &str = "type = \"gguf_vision\"\nsource = \"huggingface\"\norg = \"o\"\nrepo_name = \"r\"\nmodel = \"m.gguf\"\nmmproj = \"mm.gguf\"";

    #[rstest::rstest]
    #[case::its_cell(BenchmarkType::Eval, GGUF_TEXT, true)]
    #[case::another_benchmark(BenchmarkType::EndToEndLatency, GGUF_TEXT, false)]
    #[case::another_model_type(BenchmarkType::Eval, GGUF_VISION, false)]
    fn model_flags_match_only_their_cell(
        #[case] benchmark: BenchmarkType,
        #[case] model: &str,
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        let flags = ModelFlags::EvalGgufText {
            enable_thinking: Some(true),
            reasoning_effort: None,
        };
        let model: Model = toml::from_str(model)?;
        assert_eq!(flags.matches(benchmark, &model), expected);
        Ok(())
    }

    #[rstest::rstest]
    #[case::prefill(ModelType::GgufText, BenchmarkType::PrefillThroughput)]
    #[case::decode(ModelType::GgufText, BenchmarkType::DecodeThroughput)]
    #[case::e2e(ModelType::GgufText, BenchmarkType::EndToEndLatency)]
    #[case::afm_eval(ModelType::AppleFoundationText, BenchmarkType::Eval)]
    fn model_flag_ref_rejects_unsupported_pair(
        #[case] model_type: ModelType,
        #[case] benchmark: BenchmarkType,
    ) {
        // Apple Foundation carries no generation knobs.
        let err = ModelFlags::try_from(ModelFlagRef {
            model_type,
            benchmark_type: benchmark,
            enable_thinking: Some(true),
            reasoning_effort: None,
        });
        assert!(matches!(
            err,
            Err(ModelFlagsError::NoSuchCombination { .. })
        ));
    }

    #[test]
    fn model_flags_round_trips_through_flat_wire() -> anyhow::Result<()> {
        let flags = ModelFlags::EvalTorch {
            enable_thinking: Some(false),
            reasoning_effort: None,
        };
        let wire = toml::to_string(&flags)?;
        assert!(wire.contains(r#"model_type = "torch""#), "got: {wire}");
        assert!(wire.contains(r#"benchmark_type = "eval""#), "got: {wire}");
        let round: ModelFlags = toml::from_str(&wire)?;
        assert_eq!(flags, round);
        Ok(())
    }

    #[test]
    fn reasoning_effort_round_trips_through_flat_wire() -> anyhow::Result<()> {
        let flags: ModelFlags = toml::from_str(
            r#"model_type = "gguf_text"
benchmark_type = "eval"
reasoning_effort = "high""#,
        )?;
        assert_eq!(
            flags,
            ModelFlags::EvalGgufText {
                enable_thinking: None,
                reasoning_effort: Some(ReasoningEffort::High),
            }
        );
        let wire = toml::to_string(&flags)?;
        assert!(wire.contains(r#"reasoning_effort = "high""#), "got: {wire}");
        assert_eq!(toml::from_str::<ModelFlags>(&wire)?, flags);
        Ok(())
    }

    #[rstest::rstest]
    #[case::none("torch", "none")]
    #[case::minimal("torch", "minimal")]
    #[case::capitalized("gguf_text", "Low")]
    #[case::vision_cell("gguf_vision", "low")]
    #[case::mlx_cell("mlx", "low")]
    fn model_flags_refuse_a_reasoning_effort_they_cannot_carry(
        #[case] model_type: &str,
        #[case] effort: &str,
    ) {
        let toml_str = format!(
            "model_type = \"{model_type}\"\nbenchmark_type = \"eval\"\nreasoning_effort = \"{effort}\""
        );
        assert!(toml::from_str::<ModelFlags>(&toml_str).is_err());
    }

    #[test]
    fn model_flag_ref_rejects_unknown_field() {
        let toml_str = r#"model_type = "gguf_text"
benchmark_type = "eval"
enabel_thinking = true"#;
        assert!(toml::from_str::<ModelFlags>(toml_str).is_err());
    }

    #[test]
    fn model_flags_enable_thinking_omitted_from_wire_when_unset() -> anyhow::Result<()> {
        let wire = toml::to_string(&ModelFlags::EvalMlx {
            enable_thinking: None,
        })?;
        assert!(
            !wire.contains("enable_thinking"),
            "unset knob must not serialize, got:\n{wire}"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case(Some(true), Some("enable_thinking=true"))]
    #[case(Some(false), Some("enable_thinking=false"))]
    #[case(None, None)]
    fn model_flags_canonical_string_reflects_enable_thinking(
        #[case] enable_thinking: Option<bool>,
        #[case] expected: Option<&str>,
    ) {
        let flags = ModelFlags::EvalGgufText {
            enable_thinking,
            reasoning_effort: None,
        };
        assert_eq!(flags.canonical_string().as_deref(), expected);
    }

    #[rstest::rstest]
    #[case(
        Some(true),
        ReasoningEffort::Low,
        "enable_thinking=true,reasoning_effort=low"
    )]
    #[case(
        Some(false),
        ReasoningEffort::High,
        "enable_thinking=false,reasoning_effort=high"
    )]
    #[case(None, ReasoningEffort::Medium, "reasoning_effort=medium")]
    fn model_flags_canonical_string_appends_reasoning_effort(
        #[case] enable_thinking: Option<bool>,
        #[case] effort: ReasoningEffort,
        #[case] expected: &str,
    ) {
        let flags = ModelFlags::EvalTorch {
            enable_thinking,
            reasoning_effort: Some(effort),
        };
        assert_eq!(flags.canonical_string().as_deref(), Some(expected));
    }

    /// Stored rows group on the canonical string and its `sha256`, and job keys
    /// on the wire entry, so none of the three may move.
    #[rstest::rstest]
    #[case::thinking_on(
        Some(true),
        Some((
            "enable_thinking=true",
            "1c62eacff8d93b2e6ce0c39cbd0cd5bcf1e87f70af26c670bfbebca5e64cea9a",
        )),
        r#","enable_thinking":true}"#
    )]
    #[case::thinking_off(
        Some(false),
        Some((
            "enable_thinking=false",
            "1b03080de4603f9af160c4adc54b1153abdcadac11521a0b6981635752f32c6c",
        )),
        r#","enable_thinking":false}"#
    )]
    #[case::unset(None, None, "}")]
    fn flags_without_reasoning_effort_keep_their_spelling_and_hash(
        #[values("gguf_text", "gguf_vision", "mlx", "torch")] model: &str,
        #[case] enable_thinking: Option<bool>,
        #[case] canonical: Option<(&str, &str)>,
        #[case] wire_tail: &str,
    ) -> anyhow::Result<()> {
        use sha2::{Digest as _, Sha256};

        let flags = ModelFlags::try_from(ModelFlagRef {
            model_type: serde_json::from_value(serde_json::json!(model))?,
            benchmark_type: BenchmarkType::Eval,
            enable_thinking,
            reasoning_effort: None,
        })?;
        let spelled = flags.canonical_string();
        let hashed = spelled
            .as_deref()
            .map(|s| hex::encode(Sha256::digest(s.as_bytes())));
        assert_eq!(spelled.as_deref().zip(hashed.as_deref()), canonical);
        assert_eq!(
            serde_json::to_string(&flags)?,
            format!(r#"{{"model_type":"{model}","benchmark_type":"eval"{wire_tail}"#)
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::thinking_on("enable_thinking=true", ModelType::Torch, Some(ModelFlags::EvalTorch { enable_thinking: Some(true), reasoning_effort: None }))]
    #[case::thinking_off(" enable_thinking=false ", ModelType::GgufText, Some(ModelFlags::EvalGgufText { enable_thinking: Some(false), reasoning_effort: None }))]
    #[case::with_effort("enable_thinking=true,reasoning_effort=low", ModelType::Torch, Some(ModelFlags::EvalTorch { enable_thinking: Some(true), reasoning_effort: Some(ReasoningEffort::Low) }))]
    #[case::effort_alone("reasoning_effort=high", ModelType::GgufText, Some(ModelFlags::EvalGgufText { enable_thinking: None, reasoning_effort: Some(ReasoningEffort::High) }))]
    #[case::effort_first("reasoning_effort=medium,enable_thinking=false", ModelType::Torch, Some(ModelFlags::EvalTorch { enable_thinking: Some(false), reasoning_effort: Some(ReasoningEffort::Medium) }))]
    #[case::space_after_comma("enable_thinking=true, reasoning_effort=low", ModelType::GgufText, Some(ModelFlags::EvalGgufText { enable_thinking: Some(true), reasoning_effort: Some(ReasoningEffort::Low) }))]
    #[case::not_a_bool("enable_thinking=yes", ModelType::Torch, None)]
    #[case::not_a_level("enable_thinking=true,reasoning_effort=none", ModelType::Torch, None)]
    #[case::effort_on_mlx("enable_thinking=true,reasoning_effort=low", ModelType::Mlx, None)]
    #[case::knob_twice("enable_thinking=true,enable_thinking=false", ModelType::Torch, None)]
    #[case::trailing_comma("enable_thinking=true,", ModelType::Torch, None)]
    #[case::unknown_knob("enable_thinking=true,temperature=0.6", ModelType::Torch, None)]
    #[case::other_text("--temp 0.7", ModelType::Torch, None)]
    fn model_flags_read_back_from_their_canonical_string(
        #[case] text: &str,
        #[case] model: ModelType,
        #[case] expected: Option<ModelFlags>,
    ) {
        assert_eq!(
            ModelFlags::from_canonical_string(text, BenchmarkType::Eval, model),
            expected
        );
    }

    #[rstest::rstest]
    fn model_flags_read_back_from_the_string_they_spell(
        #[values(
            ModelType::GgufText,
            ModelType::GgufVision,
            ModelType::Mlx,
            ModelType::Torch
        )]
        model_type: ModelType,
        #[values(Some(true), Some(false), None)] enable_thinking: Option<bool>,
        #[values(
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Medium),
            Some(ReasoningEffort::High),
            None
        )]
        reasoning_effort: Option<ReasoningEffort>,
    ) {
        // A pair the cell refuses is the routing test's.
        let Ok(flags) = ModelFlags::try_from(ModelFlagRef {
            model_type,
            benchmark_type: BenchmarkType::Eval,
            enable_thinking,
            reasoning_effort,
        }) else {
            return;
        };
        let read = flags.canonical_string().and_then(|text| {
            ModelFlags::from_canonical_string(&text, BenchmarkType::Eval, model_type)
        });
        let spells_a_knob = enable_thinking.is_some() || reasoning_effort.is_some();
        assert_eq!(read, spells_a_knob.then_some(flags));
    }

    #[test]
    fn model_flags_read_back_from_a_knob_object() -> anyhow::Result<()> {
        assert_eq!(
            ModelFlags::from_cell_json(
                r#"{"enable_thinking":false}"#,
                ModelType::Mlx,
                BenchmarkType::Eval
            )?,
            Some(ModelFlags::EvalMlx {
                enable_thinking: Some(false)
            })
        );
        assert_eq!(
            ModelFlags::from_cell_json(
                r#"{"enable_thinking":true,"reasoning_effort":"low"}"#,
                ModelType::Torch,
                BenchmarkType::Eval
            )?,
            Some(ModelFlags::EvalTorch {
                enable_thinking: Some(true),
                reasoning_effort: Some(ReasoningEffort::Low),
            })
        );
        assert!(ModelFlags::from_cell_json("{}", ModelType::Mlx, BenchmarkType::Eval)?.is_none());
        assert!(matches!(
            ModelFlags::from_cell_json(
                r#"{"model_type":"mlx"}"#,
                ModelType::Mlx,
                BenchmarkType::Eval
            ),
            Err(ModelFlagsRecordError::AxisNotAccepted { key: "model_type" })
        ));
        assert!(matches!(
            ModelFlags::from_cell_json(
                r#"{"reasoning_effort":"low"}"#,
                ModelType::Mlx,
                BenchmarkType::Eval
            ),
            Err(ModelFlagsRecordError::Flags(
                ModelFlagsError::KnobNotAllowed { .. }
            ))
        ));
        Ok(())
    }

    #[test]
    fn a_canonical_string_off_an_eval_cell_reads_nothing() {
        assert_eq!(
            ModelFlags::from_canonical_string(
                "enable_thinking=true",
                BenchmarkType::PrefillThroughput,
                ModelType::GgufText
            ),
            None
        );
    }

    #[rstest::rstest]
    #[case(Some(true), Some("enable_thinking=true"))]
    #[case(Some(false), Some("enable_thinking=false"))]
    #[case(None, None)]
    fn model_flags_submission_string_eval_keeps_canonical_form(
        #[case] enable_thinking: Option<bool>,
        #[case] expected: Option<&str>,
    ) {
        let flags = ModelFlags::EvalGgufText {
            enable_thinking,
            reasoning_effort: None,
        };
        assert_eq!(
            flags.submission_string(BenchmarkType::Eval).as_deref(),
            expected
        );
    }

    /// A vlm_eval reports its thinking mode in the text eval's spelling, so the
    /// `model_flags` column reads the same for both.
    #[rstest::rstest]
    #[case(Some(true), Some("enable_thinking=true"))]
    #[case(Some(false), Some("enable_thinking=false"))]
    #[case(None, None)]
    fn model_flags_submission_string_vlm_eval_keeps_canonical_form(
        #[case] enable_thinking: Option<bool>,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let flags: ModelFlags = toml::from_str(&format!(
            "model_type = \"gguf_vision\"\nbenchmark_type = \"vlm_eval\"{}",
            enable_thinking.map_or(String::new(), |v| format!("\nenable_thinking = {v}"))
        ))?;
        assert_eq!(flags, ModelFlags::VlmEvalGgufVision { enable_thinking });
        assert_eq!(
            flags.submission_string(BenchmarkType::VlmEval).as_deref(),
            expected
        );
        assert_eq!(
            expected.and_then(|text| ModelFlags::from_canonical_string(
                text,
                BenchmarkType::VlmEval,
                ModelType::GgufVision
            )),
            enable_thinking.is_some().then_some(flags)
        );
        Ok(())
    }

    /// An asr_eval states its (always off) thinking as a vlm_eval does, and
    /// reports it in the same spelling.
    #[rstest::rstest]
    #[case(Some(false), Some("enable_thinking=false"))]
    #[case(None, None)]
    fn an_asr_eval_reports_its_thinking_as_a_vlm_eval_does(
        #[case] enable_thinking: Option<bool>,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let flags: ModelFlags = toml::from_str(&format!(
            "model_type = \"gguf_vision\"\nbenchmark_type = \"asr_eval\"{}",
            enable_thinking.map_or(String::new(), |v| format!("\nenable_thinking = {v}"))
        ))?;
        assert_eq!(flags, ModelFlags::AsrEvalGgufVision { enable_thinking });
        assert_eq!(
            flags.submission_string(BenchmarkType::AsrEval).as_deref(),
            expected
        );
        let model: Model = serde_json::from_value(serde_json::json!({
            "type": "gguf_vision", "source": "relative_files",
            "model": "m.gguf", "mmproj": "p.gguf",
        }))?;
        assert!(flags.matches(BenchmarkType::AsrEval, &model));
        assert!(!flags.matches(BenchmarkType::VlmEval, &model));
        Ok(())
    }

    #[test]
    fn an_asr_eval_refuses_reasoning_effort() {
        assert!(
            toml::from_str::<ModelFlags>(
                "model_type = \"gguf_vision\"\nbenchmark_type = \"asr_eval\"\nreasoning_effort = \"low\""
            )
            .is_err()
        );
    }

    #[test]
    fn a_vlm_eval_refuses_reasoning_effort() {
        assert!(
            toml::from_str::<ModelFlags>(
                "model_type = \"gguf_vision\"\nbenchmark_type = \"vlm_eval\"\nreasoning_effort = \"low\""
            )
            .is_err()
        );
    }

    #[rstest::rstest]
    fn model_flags_submission_string_non_eval_reports_nothing(
        #[values(
            BenchmarkType::PrefillThroughput,
            BenchmarkType::DecodeThroughput,
            BenchmarkType::EndToEndLatency,
            BenchmarkType::MaxMemoryUsage,
            BenchmarkType::VlThroughput,
            BenchmarkType::AsrThroughput
        )]
        benchmark_type: BenchmarkType,
        #[values(Some(true), Some(false), None)] enable_thinking: Option<bool>,
    ) {
        let flags = ModelFlags::EvalGgufText {
            enable_thinking,
            reasoning_effort: None,
        };
        assert_eq!(flags.submission_string(benchmark_type), None);
    }
}
