//! A plan document, expanded into the job bodies it stands for. Every
//! transport that files a TOML plan runs [`expand_document`], so each refuses
//! the same documents with the same messages.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;

use pipette_core::benchmark::Benchmark;
use pipette_core::plan::{PlanManifest, PlanSource};
use pipette_core::vault::VaultOwner;
use pipette_plan_types::{
    BenchmarkId, BenchmarkType, ImportAction, Model, PlannedImport, Sha256Error,
};
use pipette_planner::{ExpandError, Expansion, PlanParseError};
use pipette_store::PlanStore;

use crate::gated_import::DryRunImports;
use crate::plan_ingestion::{IngestContext, IngestError, RejectionError, dry_run_jobs};
use crate::submission::without_auth_tokens;

/// A plan document after expansion: the job bodies, the source to store, and
/// the planner's warnings.
#[derive(Debug)]
pub struct ExpandedDocument {
    /// The job bodies, in the planner's order.
    pub jobs: Vec<Value>,
    /// The document as submitted, with its digest. Ingestion stores it through
    /// [`with_vault_arms`].
    pub source: PlanSource,
    /// The planner's warnings, in the order it raised them.
    pub plan_warnings: Vec<String>,
    /// The type each benchmark the jobs name was expanded under, which the
    /// plan's manifest must record.
    pub benchmark_types: BTreeMap<BenchmarkId, BenchmarkType>,
}

/// A plan document that does not expand; [`DocumentError::is_caller_fault`]
/// tells the author's faults from the server's.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    /// The document is not a valid scheduler plan, or a `runtime_flags` entry
    /// sets a HuggingFace token variable to a value.
    #[error(transparent)]
    Parse(#[from] PlanParseError),
    /// The planner cannot count the cells of the plan.
    #[error(transparent)]
    Count(ExpandError),
    #[error("{}", RejectionError::TooManyJobs { count: *count, limit: *limit })]
    TooManyJobs { count: usize, limit: NonZeroUsize },
    /// The plan breaks a capability rule, or a cell does not render.
    #[error(transparent)]
    Expand(ExpandError),
    #[error(
        "the plan document names `auth_token` outside a model's token, for example in a \
         comment. Remove it: the server stores no token"
    )]
    TokenOutsideAModel,
    #[error(transparent)]
    Stored(#[from] StoredDocumentError),
    #[error("failed to digest the plan document")]
    Digest(#[from] pipette_plan_types::Sha256Error),
    #[error("plan document expansion task failed")]
    Join(#[from] tokio::task::JoinError),
}

impl DocumentError {
    pub fn is_caller_fault(&self) -> bool {
        match self {
            Self::Parse(_)
            | Self::Count(_)
            | Self::TooManyJobs { .. }
            | Self::Expand(_)
            | Self::TokenOutsideAModel => true,
            Self::Stored(_) | Self::Digest(_) | Self::Join(_) => false,
        }
    }
}

/// Each benchmark's type as `catalog` defines it.
pub fn benchmark_types(
    catalog: &HashMap<BenchmarkId, Benchmark>,
) -> BTreeMap<BenchmarkId, BenchmarkType> {
    catalog
        .iter()
        .map(|(id, benchmark)| (id.clone(), benchmark.def.benchmark_type()))
        .collect()
}

/// The first of `expanded`'s benchmarks that `catalog` no longer gives that
/// type, as when an entry is replaced between two loads of one filing.
pub fn retyped<'a>(
    expanded: &'a BTreeMap<BenchmarkId, BenchmarkType>,
    catalog: &HashMap<BenchmarkId, Benchmark>,
) -> Option<&'a BenchmarkId> {
    expanded.iter().find_map(|(id, expanded)| {
        let loaded = catalog.get(id).map(|b| b.def.benchmark_type());
        (loaded != Some(*expanded)).then_some(id)
    })
}

/// Parse and expand a plan document. A few KB can expand to thousands of
/// bodies, so the planner runs on the blocking pool under the caller's span.
pub async fn expand_document(
    limit: NonZeroUsize,
    types: BTreeMap<BenchmarkId, BenchmarkType>,
    note: Option<&str>,
    text: String,
) -> Result<ExpandedDocument, DocumentError> {
    let note = note.map(str::to_owned);
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        span.in_scope(|| expand_document_blocking(limit, types, note.as_deref(), text))
    })
    .await?
}

/// The synchronous body of [`expand_document`].
fn expand_document_blocking(
    limit: NonZeroUsize,
    types: BTreeMap<BenchmarkId, BenchmarkType>,
    note: Option<&str>,
    text: String,
) -> Result<ExpandedDocument, DocumentError> {
    let expansion = expand_text(limit, &types, &text).inspect_err(log_refusal)?;
    tracing::info!(
        note,
        jobs = expansion.bodies.len(),
        plan_warnings = expansion.warnings.len(),
        "plan document expanded"
    );

    let source = PlanSource::new(text)?;
    let benchmark_types = expansion
        .bodies
        .iter()
        .filter_map(|body| body.pointer("/spec/benchmark")?.as_str())
        .filter_map(|id| BenchmarkId::try_new(id).ok())
        .filter_map(|id| Some((id.clone(), *types.get(&id)?)))
        .collect();
    Ok(ExpandedDocument {
        jobs: expansion.bodies,
        source,
        benchmark_types,
        plan_warnings: expansion
            .warnings
            .into_iter()
            .map(|warning| warning.message)
            .collect(),
    })
}

/// Log why a filing's document was refused, never a parse message: the parser
/// quotes the document, which can hold a token.
fn log_refusal(error: &DocumentError) {
    match error {
        DocumentError::Parse(e) => {
            let reason = match e {
                PlanParseError::Invalid(_) => "invalid",
                PlanParseError::Structure(_) => "structure",
                PlanParseError::HfTokenEnv { .. } => "hf_token_env",
            };
            tracing::warn!(reason, "plan document rejected: parse failed");
        }
        DocumentError::TokenOutsideAModel => {
            tracing::warn!("plan document rejected: it names auth_token outside a model");
        }
        DocumentError::Count(_) => {
            tracing::warn!(error = %chain(error), "plan document rejected: job count failed");
        }
        DocumentError::TooManyJobs { count, limit } => tracing::warn!(
            cells = count,
            limit = limit.get(),
            "plan document rejected: over the job ceiling"
        ),
        DocumentError::Expand(_) => {
            tracing::warn!(error = %chain(error), "plan document rejected: expansion failed");
        }
        DocumentError::Stored(_) | DocumentError::Digest(_) | DocumentError::Join(_) => {}
    }
}

/// Parse and expand `text` as every filing does, logging nothing; the job ceiling
/// is checked first, so an oversized plan costs no expansion to refuse.
pub fn expand_text(
    limit: NonZeroUsize,
    types: &BTreeMap<BenchmarkId, BenchmarkType>,
    text: &str,
) -> Result<Expansion, DocumentError> {
    let plan = pipette_planner::parse(text, types)?;
    // Any `auth_token` left once model tokens are gone would reach the store,
    // which refuses it, so refuse here, before an import.
    if pipette_core::plan::names_a_token(&without_model_tokens(text)?) {
        return Err(DocumentError::TokenOutsideAModel);
    }

    let cells = pipette_planner::job_count(&plan).map_err(DocumentError::Count)?;
    if cells > limit.get() {
        return Err(DocumentError::TooManyJobs {
            count: cells,
            limit,
        });
    }

    pipette_planner::expand(&plan).map_err(DocumentError::Expand)
}

/// An error and every source under it, joined the way `anyhow` renders `{:#}`.
pub(crate) fn chain(e: &DocumentError) -> String {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Cardinal index labels: an expanded document has no names, so a job's
/// label, which [`RejectionError::Job`] quotes, is its position.
pub fn index_labels(jobs: Vec<Value>) -> Vec<(String, Value)> {
    jobs.into_iter()
        .enumerate()
        .map(|(index, body)| (index.to_string(), body))
        .collect()
}

/// The answer to a dry run of a plan document.
#[derive(Debug, serde::Serialize)]
pub struct DryRunReport {
    /// The job bodies, each without any `auth_token`. A gated model is its
    /// HuggingFace form.
    pub bodies: Vec<Value>,
    pub plan_warnings: Vec<String>,
    /// One entry for each distinct model that carries a token.
    pub imports: Vec<PlannedImport>,
}

impl From<DryRunReport> for pipette_plan_types::PlanDryRun {
    fn from(report: DryRunReport) -> Self {
        Self {
            bodies: report.bodies,
            plan_warnings: report.plan_warnings,
            imports: report.imports,
        }
    }
}

impl DryRunReport {
    /// The report on `jobs` with no checks and no import.
    pub fn unchecked(jobs: &[Value], plan_warnings: Vec<String>) -> Self {
        Self {
            bodies: jobs.iter().map(without_auth_tokens).collect(),
            plan_warnings,
            imports: Vec::new(),
        }
    }
}

/// A dry run's [`DryRunReport`], and the bodies that a revision preview counts.
#[derive(Debug)]
pub struct DocumentDryRun {
    pub report: DryRunReport,
    /// The job bodies as a real ingest stages them, with a placeholder for each
    /// model it downloads, for [`crate::campaign_admin::Revision::preview`].
    pub preview_bodies: Vec<Value>,
}

/// Run every check a submission of `jobs` runs before it downloads a model,
/// writing nothing: the same refusals as a real submission up to the download.
pub async fn dry_run(
    ctx: &IngestContext<'_>,
    owner: &VaultOwner,
    jobs: Vec<Value>,
    plan_warnings: Vec<String>,
    now: DateTime<Utc>,
) -> Result<DocumentDryRun, IngestError> {
    let jobs = index_labels(jobs);
    let cells = jobs.len();
    let (
        bodies,
        DryRunImports {
            jobs: previewed,
            entries: imports,
        },
    ) = dry_run_jobs(ctx, owner, jobs, now).await.inspect_err(|e| {
        if let IngestError::Rejected(rejection) = e {
            tracing::warn!(
                job_index = rejection.job_index(),
                reason = %rejection,
                "plan document dry run rejected a job"
            );
        }
    })?;
    if !imports.is_empty() {
        let reuse = imports
            .iter()
            .filter(|entry| matches!(entry.action, ImportAction::Reuse { .. }))
            .count();
        tracing::info!(
            fetch = imports.len() - reuse,
            reuse,
            "plan document dry run resolved its imports"
        );
    }
    tracing::info!(cells, "plan document dry run passed validation");
    Ok(DocumentDryRun {
        report: DryRunReport {
            bodies,
            plan_warnings,
            imports,
        },
        preview_bodies: previewed.into_iter().map(|job| job.body).collect(),
    })
}

const MODELS: &str = "models";

/// Why the plan document to store could not be made: always a server fault,
/// since the planner accepted it at filing. No variant quotes the parser, which
/// can quote a token.
#[derive(Debug, thiserror::Error)]
pub enum StoredDocumentError {
    #[error("the filed plan document does not parse as TOML")]
    Unparsable,
    #[error("model {model} of variant {variant} in the filed plan document is not a model")]
    NotAModel { variant: usize, model: usize },
    #[error("a model of the plan document to store cannot be written as TOML")]
    Unwritable,
    #[error("the plan document to store is not a valid scheduler plan")]
    NotAPlan,
    #[error("a model of the plan document to store holds an `auth_token`")]
    TokenLeft,
    #[error("failed to digest the plan document to store")]
    Digest(#[from] Sha256Error),
}

/// The document the store keeps: each model in `rewrites` as its vault arm,
/// any other model stripped of its token. The import step sees only models that
/// reach a job body; a variant that runs no cell can still hold a token. A
/// tokenless `source` is kept byte for byte; any other is rewritten from its
/// TOML values, losing its comments.
pub fn with_vault_arms(
    source: PlanSource,
    rewrites: &HashMap<Model, Model>,
) -> Result<PlanSource, StoredDocumentError> {
    let mut replaced = 0_usize;
    let mut dropped = 0_usize;
    let Some(text) = rewrite_gated_models(source.text(), |variant, model| {
        let model = model.without_auth_token();
        match rewrites.get(&model) {
            Some(arm) => {
                replaced += 1;
                arm.clone()
            }
            None => {
                dropped += 1;
                tracing::info!(
                    variant,
                    "plan document token dropped: the model makes no job"
                );
                model
            }
        }
    })?
    else {
        return Ok(source);
    };
    let dropped_note = match dropped {
        0 => String::new(),
        dropped => format!(" {dropped} other model token(s) removed."),
    };
    let text = format!(
        "# Filed with {replaced} model(s) replaced by their vault imports.{dropped_note}\n{text}"
    );
    // Only the models are checked, so the cells need no types.
    let plan = pipette_planner::parse(&text, &BTreeMap::new())
        .map_err(|_| StoredDocumentError::NotAPlan)?;
    if plan
        .variants
        .iter()
        .flat_map(|variant| variant.models.iter())
        .any(|model| model.auth_token().is_some())
    {
        return Err(StoredDocumentError::TokenLeft);
    }
    tracing::info!(replaced, dropped, "plan document stored with vault arms");
    Ok(PlanSource::new(text)?)
}

/// `document` with every model token removed, for an answer that echoes it.
pub fn without_model_tokens(document: &str) -> Result<String, StoredDocumentError> {
    Ok(
        rewrite_gated_models(document, |_, model| model.without_auth_token())?
            .unwrap_or_else(|| document.to_string()),
    )
}

/// `document` with each model that holds a token replaced by `rewrite`, which
/// takes the variant index and the model. `None` when no model holds a token.
fn rewrite_gated_models(
    document: &str,
    mut rewrite: impl FnMut(usize, Model) -> Model,
) -> Result<Option<String>, StoredDocumentError> {
    let mut table: toml::Table = document
        .parse()
        .map_err(|_| StoredDocumentError::Unparsable)?;
    let variants = match table.get_mut(crate::variant_edit::VARIANTS) {
        Some(toml::Value::Array(variants)) => variants.as_mut_slice(),
        _ => &mut [],
    };
    // A model that does not parse is an error, not a model to skip: a skipped
    // model could keep its token in the stored document.
    let gated = variants.iter_mut().enumerate().try_fold(
        false,
        |gated, (variant, entry)| -> Result<bool, StoredDocumentError> {
            let Some(toml::Value::Array(models)) = entry.get_mut(MODELS) else {
                return Ok(gated);
            };
            models
                .iter_mut()
                .enumerate()
                .try_fold(gated, |gated, (index, slot)| {
                    let model: Model =
                        slot.clone()
                            .try_into()
                            .map_err(|_| StoredDocumentError::NotAModel {
                                variant,
                                model: index,
                            })?;
                    if model.auth_token().is_none() {
                        return Ok(gated);
                    }
                    *slot = toml::Value::try_from(rewrite(variant, model))
                        .map_err(|_| StoredDocumentError::Unwritable)?;
                    Ok(true)
                })
        },
    )?;
    if !gated {
        return Ok(None);
    }
    toml::to_string(&table)
        .map(Some)
        .map_err(|_| StoredDocumentError::Unwritable)
}

/// What the store holds for the plan document a manifest names.
pub enum SourceRead {
    /// The plan was filed as job bodies, so it has no document.
    NoSource,
    /// The manifest names a document the store no longer holds: an older build
    /// deleted it, or its ingest outran `plan_creating_max_age_secs`.
    Collected,
    /// The stored document does not match the manifest's `source_sha256`.
    Mismatch,
    /// The document as the store keeps it ([`with_vault_arms`]).
    Present(PlanSource),
}

/// Read the plan document `manifest` names, checked against its digest. Only a
/// store failure is an error.
pub async fn read_source(
    plans: &dyn PlanStore,
    manifest: &PlanManifest,
) -> anyhow::Result<SourceRead> {
    let Some(expected) = &manifest.source_sha256 else {
        return Ok(SourceRead::NoSource);
    };
    let source = plans
        .get_plan_source(&manifest.plan_id)
        .await
        .context("failed to read the plan source")?;
    Ok(match source {
        None => SourceRead::Collected,
        Some(source) if source.sha256() != expected => SourceRead::Mismatch,
        Some(source) => SourceRead::Present(source),
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const LLAMACPP: &str = r#"runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]"#;

    const MLX: &str = r#"runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#;

    fn gguf(token: &str) -> String {
        gguf_in("gated", token)
    }

    /// A benchmark replaced by another type, or removed, since the document
    /// was expanded is named; one that kept its type is not.
    #[test]
    fn a_benchmark_retyped_since_expansion_is_named() -> anyhow::Result<()> {
        let id = BenchmarkId::try_new("acme_probe")?;
        let entry = |def: &str| Benchmark::from_toml("acme_probe", def);
        let prefill =
            entry("benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 64")?;
        let decode = entry(
            "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 64\nparameter_decode_tokens = 8",
        )?;
        let expanded = BTreeMap::from([(id.clone(), BenchmarkType::PrefillThroughput)]);
        let catalog = |b: &Benchmark| HashMap::from([(id.clone(), b.clone())]);
        assert_eq!(retyped(&expanded, &catalog(&prefill)), None);
        assert_eq!(retyped(&expanded, &catalog(&decode)), Some(&id));
        assert_eq!(retyped(&expanded, &HashMap::new()), Some(&id));
        Ok(())
    }

    fn gguf_in(repo_name: &str, token: &str) -> String {
        format!(
            r#"{{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "{repo_name}", path = "a.gguf"{token} }}"#
        )
    }

    fn speculative(token: &str) -> String {
        format!(
            r#"{{ type = "mlx_speculative", target = {{ source = "huggingface", org = "o", repo_name = "target" }}, draft = {{ source = "huggingface", org = "o", repo_name = "draft"{token} }} }}"#
        )
    }

    /// A plan document of one variant for each `(model, runtimes)` pair.
    fn document(variants: &[(&str, &str)]) -> anyhow::Result<PlanSource> {
        let blocks: String = variants
            .iter()
            .map(|(model, runtimes)| {
                format!("\n# A comment.\n[[variants]]\nrequires = [\"os:macos\"]\nmodels   = [{model}]\n{runtimes}\n")
            })
            .collect();
        Ok(PlanSource::new(format!(
            "benchmarks = [\"decode_throughput_512_100\"]\n{blocks}"
        ))?)
    }

    fn model(toml: &str) -> anyhow::Result<Model> {
        Ok(toml::from_str::<toml::Table>(&format!("m = {toml}"))?
            .remove("m")
            .context("a model")?
            .try_into()?)
    }

    fn arm(json: &str) -> anyhow::Result<Model> {
        Ok(serde_json::from_str(json)?)
    }

    const GGUF_ARM: &str = r#"{"type":"gguf_text","source":"vault_url","url":"/vault/resources/vault_1/content","principal_id":"org_01936d"}"#;

    const SPECULATIVE_ARM: &str = r#"{"type":"mlx_speculative","target":{"source":"vault_archive","url":"/vault/resources/vault_2/content","principal_id":"org_01936d"},"draft":{"source":"vault_archive","url":"/vault/resources/vault_3/content","principal_id":"org_01936d"}}"#;

    /// The models of each variant of `text`, as the planner reads them.
    fn models(text: &str) -> anyhow::Result<Vec<Vec<Model>>> {
        Ok(
            pipette_planner::parse(text, &pipette_planner::prefix_types(text))?
                .variants
                .iter()
                .map(|variant| variant.models.iter().cloned().collect())
                .collect(),
        )
    }

    #[test]
    fn a_document_without_a_token_is_kept_as_filed() -> anyhow::Result<()> {
        let source = document(&[(&gguf(""), LLAMACPP)])?;
        let stored = with_vault_arms(source.clone(), &HashMap::new())?;
        assert!(stored == source, "{}", stored.text());
        Ok(())
    }

    /// A model the import did not see loses its token, and no token is left.
    #[rstest]
    #[case::gguf(&gguf(r#", auth_token = "hf_one""#), LLAMACPP, &gguf(r#", auth_token = "hf_one""#), GGUF_ARM)]
    #[case::another_token(&gguf(r#", auth_token = "hf_two""#), LLAMACPP, &gguf(r#", auth_token = "hf_one""#), GGUF_ARM)]
    #[case::speculative_draft(&speculative(r#", auth_token = "hf_one""#), MLX, &speculative(r#", auth_token = "hf_one""#), SPECULATIVE_ARM)]
    fn an_imported_model_is_stored_as_its_vault_arm(
        #[case] filed: &str,
        #[case] runtimes: &str,
        #[case] imported: &str,
        #[case] vault_arm: &str,
    ) -> anyhow::Result<()> {
        let dead = gguf_in("unrun", r#", auth_token = "hf_dead""#);
        let source = document(&[(filed, runtimes), (&dead, LLAMACPP)])?;
        let rewrites = HashMap::from([(model(imported)?.without_auth_token(), arm(vault_arm)?)]);

        let stored = with_vault_arms(source, &rewrites)?;

        let text = stored.text();
        assert!(!text.contains("auth_token"), "{text}");
        assert!(!text.contains("hf_"), "{text}");
        assert!(
            text.starts_with("# Filed with 1 model(s) replaced"),
            "{text}"
        );
        assert_eq!(
            models(text)?,
            [vec![arm(vault_arm)?], vec![model(&gguf_in("unrun", ""))?]]
        );
        Ok(())
    }

    /// The message quotes no token, and a model's own token is no such name.
    #[rstest]
    #[case::comment(&format!("# auth_token = \"hf_SECRET\"\n{}", document(&[(&gguf(""), LLAMACPP)])?.text()), true)]
    #[case::string(&document(&[(&gguf_in("auth_token", ""), LLAMACPP)])?.text().to_owned(), true)]
    #[case::model_token(&document(&[(&gguf(r#", auth_token = "hf_SECRET""#), LLAMACPP)])?.text().to_owned(), false)]
    fn a_token_outside_a_model_is_refused(
        #[case] text: &str,
        #[case] refused: bool,
    ) -> anyhow::Result<()> {
        let outcome = expand_document_blocking(
            NonZeroUsize::MAX,
            pipette_planner::prefix_types(text),
            None,
            text.to_owned(),
        );
        match outcome {
            Err(DocumentError::TokenOutsideAModel) => {
                assert!(refused, "refused {text}");
                let message = DocumentError::TokenOutsideAModel.to_string();
                assert!(!message.contains("SECRET"), "{message}");
            }
            Err(other) => anyhow::bail!("{other:#}"),
            Ok(_) => assert!(!refused, "accepted {text}"),
        }
        Ok(())
    }

    #[rstest]
    #[case::token(speculative(r#", auth_token = "hf_one""#), MLX, speculative(""), false)]
    #[case::plain(gguf(""), LLAMACPP, gguf(""), true)]
    fn an_echoed_document_has_no_model_token(
        #[case] filed: String,
        #[case] runtimes: &str,
        #[case] kept: String,
        #[case] unchanged: bool,
    ) -> anyhow::Result<()> {
        let source = document(&[(&filed, runtimes)])?;
        let echoed = without_model_tokens(source.text())?;
        assert!(!echoed.contains("hf_one"), "{echoed}");
        assert_eq!(models(&echoed)?, [vec![model(&kept)?]]);
        assert_eq!(echoed == source.text(), unchanged, "{echoed}");
        Ok(())
    }
}
