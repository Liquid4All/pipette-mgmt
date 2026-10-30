//! A variant edit: the change a campaign revision makes to the plan document it
//! revises, by dropping `[[variants]]` blocks and appending new ones. It edits
//! TOML values only; the planner validates the result when it expands it.

use std::collections::BTreeSet;

use serde::Deserialize;

use pipette_plan_types::RestoredVariant;

pub(crate) const VARIANTS: &str = "variants";
const BENCHMARKS: &str = "benchmarks";

/// Which variants a revision drops, and which it appends.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct VariantEdit {
    /// Indices into the current document's `[[variants]]`, numbered from 0 as
    /// the planner's messages number them.
    #[serde(default)]
    pub remove: Vec<usize>,
    /// Blocks appended after the variants that stay, in order.
    #[serde(default, rename = "variants")]
    #[cfg_attr(
        feature = "schemars",
        schemars(with = "Vec<pipette_planner::SchedulerVariant>")
    )]
    pub add: Vec<toml::Table>,
    /// The variants of other plans among `add`, which the revision records.
    /// The surface sets it from `restore`; the body cannot.
    #[serde(skip)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    pub restored: Vec<RestoredVariant>,
}

/// Why an edit does not apply. The arms from [`VariantEditError::Stored`] on
/// are server faults: the planner accepted the stored document at filing.
#[derive(Debug, thiserror::Error)]
pub enum VariantEditError {
    /// The message quotes the edit, so it goes to the caller, not to a log.
    #[error("the variant edit is not valid: {0}")]
    Invalid(#[from] toml::de::Error),
    #[error("the variant edit neither adds nor removes a variant")]
    Empty,
    #[error("variant {index} does not exist: the plan has {count}, numbered from 0")]
    NoSuchVariant { index: usize, count: usize },
    #[error("variant {0} is removed twice")]
    RemovedTwice(usize),
    #[error("the variant edit removes every variant and adds none; a plan keeps at least one")]
    NoVariantLeft,
    #[error("the stored plan document does not parse")]
    Stored(#[source] toml::de::Error),
    #[error("the stored plan document has no `variants` array")]
    NoVariants,
    #[error("the revised plan document cannot be written")]
    Unwritable(#[from] toml::ser::Error),
}

impl VariantEditError {
    pub fn is_caller_fault(&self) -> bool {
        !matches!(
            self,
            Self::Stored(_) | Self::NoVariants | Self::Unwritable(_)
        )
    }
}

impl VariantEdit {
    pub fn parse(text: &str) -> Result<Self, VariantEditError> {
        Ok(toml::from_str(text)?)
    }

    /// This edit, appending `variants` after its own.
    pub fn adding(mut self, variants: Vec<toml::Table>) -> Self {
        self.add.extend(variants);
        self
    }

    /// This method appends `variants` after the edit's variants. It records
    /// them in `restored` in the same order.
    pub fn restoring(mut self, variants: Vec<toml::Table>, restored: Vec<RestoredVariant>) -> Self {
        self.restored.extend(restored);
        self.adding(variants)
    }

    /// `document` with this edit applied: the kept variants in order, then the
    /// added ones. Re-serialized from TOML values, so comments and layout go.
    pub fn apply(&self, document: &str) -> Result<String, VariantEditError> {
        if self.remove.is_empty() && self.add.is_empty() {
            return Err(VariantEditError::Empty);
        }
        let mut table = parse_stored(document)?;
        let variants = variants_mut(&mut table)?;

        let count = variants.len();
        let removed = self
            .remove
            .iter()
            .try_fold(BTreeSet::new(), |mut removed, &index| {
                if index >= count {
                    return Err(VariantEditError::NoSuchVariant { index, count });
                }
                if !removed.insert(index) {
                    return Err(VariantEditError::RemovedTwice(index));
                }
                Ok(removed)
            })?;
        let kept = std::mem::take(variants)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !removed.contains(index))
            .map(|(_, variant)| variant);
        *variants = kept
            .chain(self.add.iter().cloned().map(toml::Value::Table))
            .collect();
        if variants.is_empty() {
            return Err(VariantEditError::NoVariantLeft);
        }
        Ok(toml::to_string(&table)?)
    }
}

/// Each variant of `document` as its own `[[variants]]` block, under a
/// `# variant {index}` line: what an operator reads to choose what to remove.
pub fn numbered_variants(document: &str) -> Result<String, VariantEditError> {
    Ok(variant_blocks(document)?
        .iter()
        .enumerate()
        .map(|(index, block)| format!("# variant {index}\n{block}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Each variant of `document` as its own `[[variants]]` block, in the order
/// `remove` numbers them.
pub fn variant_blocks(document: &str) -> Result<Vec<String>, VariantEditError> {
    let mut table = parse_stored(document)?;
    let variants = std::mem::take(variants_mut(&mut table)?);
    variants.into_iter().map(variant_block).collect()
}

fn variant_block(variant: toml::Value) -> Result<String, VariantEditError> {
    let block = toml::Table::from_iter([(VARIANTS.to_string(), toml::Value::Array(vec![variant]))]);
    Ok(toml::to_string(&block)?)
}

/// The variants of `document`, each given the plan's `benchmarks` where it sets
/// none, so it expands to the same cells in another plan's document.
pub fn standalone_variants(document: &str) -> Result<Vec<toml::Table>, VariantEditError> {
    let mut table = parse_stored(document)?;
    let benchmarks = table.get(BENCHMARKS).cloned();
    Ok(std::mem::take(variants_mut(&mut table)?)
        .into_iter()
        .filter_map(|variant| {
            let toml::Value::Table(mut variant) = variant else {
                return None;
            };
            if let Some(benchmarks) = &benchmarks {
                variant
                    .entry(BENCHMARKS)
                    .or_insert_with(|| benchmarks.clone());
            }
            Some(variant)
        })
        .collect())
}

/// Each of [`standalone_variants`] as its own `[[variants]]` block, in the
/// order that a variant edit's `restore` numbers them.
pub fn standalone_variant_blocks(document: &str) -> Result<Vec<String>, VariantEditError> {
    standalone_variants(document)?
        .into_iter()
        .map(toml::Value::Table)
        .map(variant_block)
        .collect()
}

/// [`standalone_variants`] as `[[variants]]` blocks, for another document to
/// append.
pub fn standalone_blocks(document: &str) -> Result<String, VariantEditError> {
    let variants = standalone_variants(document)?
        .into_iter()
        .map(toml::Value::Table)
        .collect();
    let blocks = toml::Table::from_iter([(VARIANTS.to_string(), toml::Value::Array(variants))]);
    Ok(toml::to_string(&blocks)?)
}

/// The indices of `document`'s variants that repeat an earlier one, once each
/// holds its benchmark list: they describe no cell the earlier one does not.
pub fn repeated_variants(document: &str) -> Result<Vec<usize>, VariantEditError> {
    let variants = standalone_variants(document)?;
    Ok((0..variants.len())
        .filter(|&index| {
            variants.get(index).is_some_and(|variant| {
                variants
                    .get(..index)
                    .is_some_and(|earlier| earlier.contains(variant))
            })
        })
        .collect())
}

fn parse_stored(document: &str) -> Result<toml::Table, VariantEditError> {
    document.parse().map_err(VariantEditError::Stored)
}

fn variants_mut(table: &mut toml::Table) -> Result<&mut Vec<toml::Value>, VariantEditError> {
    match table.get_mut(VARIANTS) {
        Some(toml::Value::Array(variants)) => Ok(variants),
        _ => Err(VariantEditError::NoVariants),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const AFM_MLX: &str = include_str!("../../../examples/plans/scheduler/afm-mlx.toml");

    /// A third variant, told apart from the example's two by its `requires`.
    const LINUX_VARIANT: &str = r#"
[[variants]]
requires = ["os:linux"]
models   = [{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-350M-GGUF", path = "LFM2-350M-Q4_0.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "linux-x64" }]
benchmarks = ["decode_throughput_512_100"]
"#;

    fn requires_of(document: &str) -> anyhow::Result<Vec<String>> {
        let plan = pipette_planner::parse(document, &pipette_planner::prefix_types(document))?;
        Ok(plan
            .variants
            .iter()
            .map(|variant| {
                variant
                    .requires
                    .iter()
                    .map(|flag| flag.as_ref().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect())
    }

    /// The result is a plan the planner accepts with the same top-level fields.
    #[test]
    fn an_edit_keeps_the_rest_in_order_and_appends() -> anyhow::Result<()> {
        let edit = VariantEdit::parse(&format!("remove = [0]\n{LINUX_VARIANT}"))?;
        let revised = edit.apply(AFM_MLX)?;

        assert_eq!(requires_of(&revised)?, vec!["os:macos", "os:linux"]);
        let (before, after) = (
            pipette_planner::parse(AFM_MLX, &pipette_planner::prefix_types(AFM_MLX))?,
            pipette_planner::parse(&revised, &pipette_planner::prefix_types(&revised))?,
        );
        assert_eq!(after.benchmarks, before.benchmarks);
        assert_eq!(
            after.variants.first(),
            before.variants.get(1),
            "a kept variant is unchanged"
        );
        Ok(())
    }

    #[rstest]
    #[case::add_only(LINUX_VARIANT.to_string(), &["os:ios", "os:macos", "os:linux"])]
    #[case::remove_only("remove = [1]".to_string(), &["os:ios"])]
    fn each_half_is_an_edit_on_its_own(
        #[case] edit: String,
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let revised = VariantEdit::parse(&edit)?.apply(AFM_MLX)?;
        assert_eq!(requires_of(&revised)?, expected);
        Ok(())
    }

    /// Which refusal a case expects. A fn pointer, because the arms carry
    /// payloads that do not compare by equality.
    type Refusal = fn(&VariantEditError) -> bool;

    #[rstest]
    #[case::empty("", (|e| matches!(e, VariantEditError::Empty)) as Refusal)]
    #[case::out_of_range("remove = [2]", (|e| matches!(e, VariantEditError::NoSuchVariant { index: 2, count: 2 })) as Refusal)]
    #[case::twice("remove = [1, 1]", (|e| matches!(e, VariantEditError::RemovedTwice(1))) as Refusal)]
    #[case::everything("remove = [0, 1]", (|e| matches!(e, VariantEditError::NoVariantLeft)) as Refusal)]
    fn an_edit_that_cannot_apply_is_refused(
        #[case] edit: &str,
        #[case] expected: Refusal,
    ) -> anyhow::Result<()> {
        let Err(error) = VariantEdit::parse(edit)?.apply(AFM_MLX) else {
            anyhow::bail!("expected {edit:?} to be refused");
        };
        assert!(expected(&error), "{error:?}");
        assert!(error.is_caller_fault());
        Ok(())
    }

    /// A misspelt key cannot be silently dropped.
    #[rstest]
    #[case::unknown_key("removes = [0]")]
    #[case::plan_field("benchmarks = [\"decode_throughput_512_100\"]")]
    #[case::negative_index("remove = [-1]")]
    fn an_edit_document_takes_two_keys(#[case] edit: &str) {
        assert!(matches!(
            VariantEdit::parse(edit),
            Err(VariantEditError::Invalid(_))
        ));
    }

    /// Each block reads back as the variant it came from.
    #[test]
    fn the_listing_numbers_variants_as_remove_counts_them() -> anyhow::Result<()> {
        let listing = numbered_variants(AFM_MLX)?;
        let blocks: Vec<&str> = listing.split("# variant ").skip(1).collect();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("0\n"), "{listing}");
        assert!(blocks[1].starts_with("1\n"), "{listing}");
        assert!(blocks[1].contains("os:macos"), "{listing}");
        Ok(())
    }

    /// A variant naming its own `benchmarks` keeps them.
    #[test]
    fn a_standalone_variant_takes_its_plans_benchmarks() -> anyhow::Result<()> {
        let benchmarks = |variant: &toml::Table| variant.get(BENCHMARKS).cloned();
        let plan_wide =
            toml::Value::try_from(["decode_throughput_512_100", "end_to_end_latency_512_256"])?;
        let own = toml::Value::try_from(["decode_throughput_512_100"])?;

        let variants = standalone_variants(AFM_MLX)?;
        assert_eq!(
            variants.iter().map(benchmarks).collect::<Vec<_>>(),
            vec![Some(own), Some(plan_wide)]
        );

        let without_list = AFM_MLX.replacen("benchmarks = [", "listed = [", 1);
        let variants = standalone_variants(&without_list)?;
        assert_eq!(variants.get(1).and_then(benchmarks), None);
        Ok(())
    }

    #[test]
    fn standalone_blocks_parse_as_the_standalone_variants() -> anyhow::Result<()> {
        let blocks = standalone_blocks(AFM_MLX)?;
        assert_eq!(standalone_variants(&blocks)?, standalone_variants(AFM_MLX)?);
        Ok(())
    }

    #[test]
    fn each_standalone_variant_block_holds_its_variant() -> anyhow::Result<()> {
        let variants = standalone_variants(AFM_MLX)?;
        let blocks = standalone_variant_blocks(AFM_MLX)?;
        assert_eq!(blocks.len(), variants.len());
        blocks
            .iter()
            .zip(&variants)
            .try_for_each(|(block, variant)| {
                assert_eq!(&standalone_variants(block)?, std::slice::from_ref(variant));
                anyhow::Ok(())
            })
    }

    /// A variant that names the plan's list is the one that inherits it, so a
    /// copy with the list filled in repeats it; another variant does not.
    #[test]
    fn a_variant_that_holds_the_same_cells_is_repeated() -> anyhow::Result<()> {
        let copied = standalone_blocks(AFM_MLX)?;
        let document = format!("{AFM_MLX}{LINUX_VARIANT}\n{copied}");

        assert_eq!(repeated_variants(&document)?, vec![3, 4]);
        assert!(repeated_variants(AFM_MLX)?.is_empty());
        Ok(())
    }
}
