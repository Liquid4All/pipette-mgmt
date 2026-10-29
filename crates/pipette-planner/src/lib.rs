//! Server-side expansion of a scheduler-mode plan document; no IO.
//!
//! Ingestion merges jobs by content key, so a body from [`expand`] must equal
//! the one `pipette-plan generate` wrote for the same plan (`tests/parity.rs`).
//! The client no longer expands plans (PIP-633), so a rule change is made here
//! alone.

use std::collections::{BTreeMap, BTreeSet};

use pipette_plan_types::{BenchmarkId, BenchmarkType};

mod capability_rules;
mod expand;
mod scheduler_plan;

pub use capability_rules::{CapabilityRuleError, VariantRuleError};
pub use expand::{ExpandError, Expansion, PlanWarning, expand, job_count};
pub use scheduler_plan::{
    Eligibility, PlanExpiresAt, SchedulerCell, SchedulerPlan, SchedulerPlanError, SchedulerVariant,
};

const HF_TOKEN_ENV_NAMES: [&str; 3] = [
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    pipette_plan_types::HF_TOKEN_ENV,
];

/// A document that is not a valid scheduler plan. No message echoes a secret.
#[derive(Debug, thiserror::Error)]
pub enum PlanParseError {
    /// The message leaves out the source, so a `{:#}` chain prints the
    /// diagnostic once.
    #[error("the document is not a valid scheduler plan")]
    Invalid(#[from] toml::de::Error),
    #[error("the document is not a valid scheduler plan")]
    Structure(#[from] SchedulerPlanError),
    /// The bare `NAME` form, which inherits the device's own value, is allowed.
    #[error(
        "variant {variant}: `runtime_flags` sets {name} to a value; remove the \
         value, and set PIPETTE_HF_TOKEN on the devices that run the plan"
    )]
    HfTokenEnv { variant: usize, name: String },
}

/// Rejects a HuggingFace token value in `runtime_flags`: every device that runs
/// the job gets its environment. A model `auth_token` is fine, since the server
/// imports that model into its vault and stores no token.
///
/// `types` gives each benchmark its catalog type: the filer's catalog at
/// ingestion, the plan's stored `benchmark_types` after. A cell of an id it
/// lacks is untyped and takes no flags.
pub fn parse(
    document: &str,
    types: &BTreeMap<BenchmarkId, BenchmarkType>,
) -> Result<SchedulerPlan, PlanParseError> {
    // Two steps rather than serde's `try_from`, which would flatten the
    // structural fault into a string.
    let intake: scheduler_plan::SchedulerPlanIntake = toml::from_str(document)?;
    let plan = SchedulerPlan::from_intake(intake, types)?;
    plan.variants
        .iter()
        .enumerate()
        .try_for_each(|(variant, v)| reject_token_envs(variant, v))?;
    Ok(plan)
}

/// Every benchmark id `document` names, at the root or in a variant. Unchecked
/// beyond TOML shape, so a document that would not parse may still list them.
pub fn benchmark_ids(document: &str) -> Result<BTreeSet<BenchmarkId>, PlanParseError> {
    let intake: scheduler_plan::SchedulerPlanIntake = toml::from_str(document)?;
    Ok(intake.benchmark_ids().cloned().collect())
}

/// Each of `document`'s ids typed by its prefix, as plans were filed before
/// their manifest recorded `benchmark_types`. For tests: a server parse takes
/// the catalog's types or the manifest's.
pub fn prefix_types(document: &str) -> BTreeMap<BenchmarkId, BenchmarkType> {
    benchmark_ids(document)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| Some((id.clone(), pipette_plan_types::benchmark_type_of(&id)?)))
        .collect()
}

/// [`parse`] typing by prefix, for this crate's tests.
#[cfg(test)]
pub(crate) fn parse_prefixed(document: &str) -> Result<SchedulerPlan, PlanParseError> {
    parse(document, &prefix_types(document))
}

fn reject_token_envs(variant: usize, v: &SchedulerVariant) -> Result<(), PlanParseError> {
    match v
        .runtime_flags
        .iter()
        .flat_map(|flags| flags.envs())
        .find(|env| env.value().is_some() && HF_TOKEN_ENV_NAMES.contains(&env.name()))
    {
        Some(env) => Err(PlanParseError::HfTokenEnv {
            variant,
            name: env.name().to_string(),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use super::parse_prefixed as parse;
    use super::*;

    const MLX_VARIANT: &str = r#"
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#;

    #[test]
    fn a_valid_document_parses() -> anyhow::Result<()> {
        let plan = parse(&format!(
            "benchmarks = [\"decode_throughput_512_100\"]\n{MLX_VARIANT}"
        ))?;
        assert_eq!(plan.variants.len(), 1);
        Ok(())
    }

    /// Each rejection carries the parser's own message, so the author can find
    /// the fault.
    #[rstest]
    #[case::bad_syntax("benchmarks = [", "missing comma")]
    #[case::unknown_field("plan_id = \"x\"\n", "unknown field `plan_id`")]
    #[case::no_benchmarks("", "benchmarks")]
    fn an_invalid_document_is_rejected(
        #[case] prefix: &str,
        #[case] needle: &str,
    ) -> anyhow::Result<()> {
        let Err(error @ (PlanParseError::Invalid(_) | PlanParseError::Structure(_))) =
            parse(&format!("{prefix}{MLX_VARIANT}"))
        else {
            anyhow::bail!("expected the document to be rejected");
        };
        let message = format!("{:#}", anyhow::Error::new(error));
        anyhow::ensure!(message.contains(needle), "{needle:?} not in {message}");
        Ok(())
    }

    const MLX_MODEL: &str = r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }"#;

    fn second_variant(lines: &str, model: &str) -> String {
        format!(
            r#"benchmarks = ["decode_throughput_512_100"]
{MLX_VARIANT}
[[variants]]
{lines}
models   = [{model}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
        )
    }

    /// A structural fault keeps its type and the index of its variant.
    #[rstest]
    #[case::no_eligibility("", MLX_MODEL)]
    #[case::empty_not_all_of_group("requires = [\"os:macos\"]\nnot_all_of = [[]]", MLX_MODEL)]
    #[case::duplicate_client_version(
        "requires = [\"os:macos\"]\nclient_versions = [\"1.0\", \"1.0\"]",
        MLX_MODEL
    )]
    #[case::orphan_model("requires = [\"os:macos\"]", GGUF_MODEL)]
    fn a_structural_fault_names_its_variant(
        #[case] lines: &str,
        #[case] model: &str,
    ) -> anyhow::Result<()> {
        let Err(PlanParseError::Structure(error)) = parse(&second_variant(lines, model)) else {
            anyhow::bail!("expected a structural fault");
        };
        assert_eq!(error.index(), 1, "{error}");
        Ok(())
    }

    /// The llama.cpp variant sits at index 1, so a rejection must name it.
    fn llamacpp_plan(model: &str, envs: &str) -> String {
        format!(
            r#"benchmarks = ["prefill_throughput_512"]
{MLX_VARIANT}
[[variants]]
requires = ["os:macos"]
models   = [{model}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }}]
runtime_flags = [{{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", envs = [{envs}] }}]
"#
        )
    }

    const GGUF_MODEL: &str = r#"{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }"#;
    const GATED_GGUF_MODEL: &str = r#"{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf", auth_token = "hf_supersecret" }"#;

    /// The message names the variant and `PIPETTE_HF_TOKEN`, never the token.
    #[rstest]
    #[case::hf_token(r#""HF_TOKEN=hf_supersecret""#, "HF_TOKEN")]
    #[case::hub_token(r#""HUGGING_FACE_HUB_TOKEN=hf_supersecret""#, "HUGGING_FACE_HUB_TOKEN")]
    #[case::pipette_token(
        r#""GGML_METAL=1", "PIPETTE_HF_TOKEN=hf_supersecret""#,
        "PIPETTE_HF_TOKEN"
    )]
    fn a_token_env_value_is_rejected(
        #[case] envs: &str,
        #[case] env_name: &str,
    ) -> anyhow::Result<()> {
        let Err(error) = parse(&llamacpp_plan(GGUF_MODEL, envs)) else {
            anyhow::bail!("expected the document to be rejected");
        };
        let PlanParseError::HfTokenEnv { variant: 1, name } = &error else {
            anyhow::bail!("unexpected rejection {error:?}");
        };
        anyhow::ensure!(name == env_name, "{name}");
        let message = error.to_string();
        anyhow::ensure!(message.starts_with("variant 1: "), "{message}");
        anyhow::ensure!(
            message.contains("PIPETTE_HF_TOKEN on the devices"),
            "{message}"
        );
        anyhow::ensure!(!message.contains("hf_supersecret"), "{message}");
        Ok(())
    }

    /// On a single-source model and on the draft of a speculative model.
    #[rstest]
    #[case::gguf(&llamacpp_plan(GATED_GGUF_MODEL, ""))]
    #[case::speculative_draft(&format!(
        r#"benchmarks = ["decode_throughput_512_100"]
{MLX_VARIANT}
[[variants]]
requires = ["os:macos"]
models   = [{{ type = "mlx_speculative", target = {{ source = "huggingface", org = "o", repo_name = "target" }}, draft = {{ source = "huggingface", org = "o", repo_name = "draft", auth_token = "hf_supersecret" }} }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
    ))]
    fn a_model_token_parses(#[case] document: &str) -> anyhow::Result<()> {
        let plan = parse(document)?;
        let gated = plan
            .variants
            .get(1)
            .context("a second variant")?
            .models
            .iter()
            .any(|model| model.auth_token().is_some());
        anyhow::ensure!(gated, "the model keeps its token");
        Ok(())
    }

    /// The bare-name form inherits the device's own value, so it holds no
    /// secret.
    #[rstest]
    #[case::inherit_hf_token(r#""HF_TOKEN""#)]
    #[case::inherit_pipette_token(r#""PIPETTE_HF_TOKEN""#)]
    #[case::unrelated_value(r#""GGML_METAL=1""#)]
    fn a_document_without_a_token_value_parses(#[case] envs: &str) -> anyhow::Result<()> {
        let plan = parse(&llamacpp_plan(GGUF_MODEL, envs))?;
        assert_eq!(plan.variants.len(), 2);
        Ok(())
    }
}
