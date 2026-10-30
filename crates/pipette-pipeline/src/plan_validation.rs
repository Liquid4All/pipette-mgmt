//! The verdict of a plan check: a plan document checked as filing checks it
//! before its import, each refusal placed from its typed error while it still
//! has a span, variant or benchmark.

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashSet};
use std::num::NonZeroUsize;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use toml::de::{DeTable, DeValue};

use pipette_plan_types::{BenchmarkId, BenchmarkType};
use pipette_planner::{Expansion, PlanParseError, PlanWarning};

use crate::plan_document;
use crate::plan_document::{DocumentError, chain, index_labels};
use crate::plan_ingestion::{
    IngestContext, IngestError, JobRejectionError, RejectionError, validate_submission,
};
use crate::submission::without_auth_tokens;
use crate::variant_edit::VARIANTS;

/// The verdict on a plan document. The document is valid when no issue is an
/// error.
#[derive(Debug, Serialize)]
pub struct PlanValidation {
    /// The jobs the document expands to, after equal bodies merge. `None` when
    /// it does not expand.
    pub job_count: Option<usize>,
    /// The distinct models that carry an `auth_token`. Filing imports them and
    /// checks their access; this check does neither. `None` when `job_count` is.
    pub gated_model_count: Option<usize>,
    /// Errors, then the planner's warnings in plan order; today at most one error.
    pub issues: Vec<PlanIssue>,
}

/// One fault in a plan document.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct PlanIssue {
    pub severity: IssueSeverity,
    pub message: String,
    /// The 1-based line in the submitted text.
    pub line: Option<usize>,
    /// The 1-based column, in characters. Only a TOML fault has one.
    pub column: Option<usize>,
}

/// Whether an issue stops the document from being filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
}

impl From<PlanValidation> for pipette_plan_types::PlanValidation {
    fn from(validation: PlanValidation) -> Self {
        Self {
            job_count: validation.job_count,
            gated_model_count: validation.gated_model_count,
            issues: validation.issues.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PlanIssue> for pipette_plan_types::PlanIssue {
    fn from(issue: PlanIssue) -> Self {
        Self {
            severity: match issue.severity {
                IssueSeverity::Error => pipette_plan_types::IssueSeverity::Error,
                IssueSeverity::Warning => pipette_plan_types::IssueSeverity::Warning,
            },
            message: issue.message,
            line: issue.line,
            column: issue.column,
        }
    }
}

impl PlanIssue {
    fn error(message: String, line: Option<usize>, column: Option<usize>) -> Self {
        Self {
            severity: IssueSeverity::Error,
            message,
            line,
            column,
        }
    }
}

/// How far [`expand`] took a document.
pub enum Stage {
    /// The document does not expand. The verdict holds the one error.
    Refused(PlanValidation),
    /// The document expands, and [`check`] judges its jobs.
    Expanded { text: String, expansion: Expansion },
}

/// Parse and expand `text` on the blocking pool, as filing does. A fault of the
/// document is a [`Stage::Refused`]; a fault of the server is an error.
pub async fn expand(
    limit: NonZeroUsize,
    types: BTreeMap<BenchmarkId, BenchmarkType>,
    text: String,
) -> Result<Stage, DocumentError> {
    tokio::task::spawn_blocking(move || stage(limit, types, text)).await?
}

/// The synchronous body of [`expand`].
fn stage(
    limit: NonZeroUsize,
    types: BTreeMap<BenchmarkId, BenchmarkType>,
    text: String,
) -> Result<Stage, DocumentError> {
    match plan_document::expand_text(limit, &types, &text) {
        Ok(expansion) => Ok(Stage::Expanded { text, expansion }),
        Err(e) => refusal(&text, e).map(Stage::Refused),
    }
}

/// The verdict on `text` when `error` is its author's fault, or `error` when it
/// is the server's: the rule filing answers `400` or `500` by.
fn refusal(text: &str, error: DocumentError) -> Result<PlanValidation, DocumentError> {
    if !error.is_caller_fault() {
        return Err(error);
    }
    Ok(PlanValidation {
        job_count: None,
        gated_model_count: None,
        issues: vec![issue_for(text, &error)],
    })
}

/// The verdict on the expanded jobs of `text` in `ctx`'s scope: a rejection is an
/// issue, a store failure an error.
pub async fn check(
    ctx: &IngestContext<'_>,
    text: String,
    expansion: Expansion,
    now: DateTime<Utc>,
) -> Result<PlanValidation, IngestError> {
    let Expansion { bodies, warnings } = expansion;
    let job_count = bodies.len();
    let gated_model_count = gated_models(&bodies);
    let rejection = match validate_submission(ctx, &index_labels(bodies), now).await {
        Ok(()) => None,
        Err(IngestError::Rejected(rejection)) => Some(rejection),
        Err(e) => return Err(e),
    };
    // Placing an issue parses the whole document, so it runs off the executor.
    let issues = if rejection.is_none() && warnings.is_empty() {
        Vec::new()
    } else {
        tokio::task::spawn_blocking(move || place(&text, rejection, warnings))
            .await
            .map_err(anyhow::Error::from)?
    };
    Ok(PlanValidation {
        job_count: Some(job_count),
        gated_model_count: Some(gated_model_count),
        issues,
    })
}

fn place(
    text: &str,
    rejection: Option<RejectionError>,
    warnings: Vec<PlanWarning>,
) -> Vec<PlanIssue> {
    let document = Document::new(text);
    rejection
        .map(|rejection| rejection_issue(&document, rejection))
        .into_iter()
        .chain(warnings.into_iter().map(|warning| PlanIssue {
            severity: IssueSeverity::Warning,
            line: document.variant_line(warning.variant),
            column: None,
            message: warning.message,
        }))
        .collect()
}

/// A rejection of the expanded jobs as an issue. A job's index is a position
/// in the expansion, not in the document, so the message leaves it out.
fn rejection_issue(document: &Document<'_>, rejection: RejectionError) -> PlanIssue {
    match rejection {
        RejectionError::Job { kind, .. } => {
            // Only a benchmark id is a literal of the document to point at.
            let line = if let JobRejectionError::UnknownBenchmark { id, .. } = &kind {
                document.benchmark_line(id.as_str())
            } else {
                None
            };
            PlanIssue::error(kind.to_string(), line, None)
        }
        RejectionError::NoJobs
        | RejectionError::TooManyJobs { .. }
        | RejectionError::RerunDrift { .. } => PlanIssue::error(rejection.to_string(), None, None),
    }
}

/// The issue `error` is to the author of `text`.
fn issue_for(text: &str, error: &DocumentError) -> PlanIssue {
    let document = Document::new(text);
    let variant = match error {
        DocumentError::Parse(PlanParseError::Invalid(e)) => {
            // toml places a fault of the root table at 0..0, which would read
            // as a misleading 1:1.
            let (line, column) = e
                .span()
                .filter(|span| span.end > 0)
                .and_then(|span| document.position(span.start))
                .unzip();
            return PlanIssue::error(e.message().to_owned(), line, column);
        }
        DocumentError::Parse(PlanParseError::Structure(e)) => Some(e.index()),
        DocumentError::Parse(PlanParseError::HfTokenEnv { variant, .. }) => Some(*variant),
        DocumentError::Count(e) | DocumentError::Expand(e) => e.variant(),
        // A fault of the whole document, or of the server, names no variant.
        DocumentError::TooManyJobs { .. }
        | DocumentError::TokenOutsideAModel
        | DocumentError::Stored(_)
        | DocumentError::Digest(_)
        | DocumentError::Join(_) => None,
    };
    let line = variant.and_then(|index| document.variant_line(index));
    PlanIssue::error(chain(error), line, None)
}

/// The distinct models, tokens aside, that carry an `auth_token` in `jobs`.
fn gated_models(jobs: &[Value]) -> usize {
    jobs.iter()
        .filter_map(|body| body.pointer("/spec/model"))
        .filter_map(|model| {
            let bare = without_auth_tokens(model);
            (bare != *model).then(|| bare.to_string())
        })
        .collect::<HashSet<_>>()
        .len()
}

/// The submitted text, and the line starts and TOML tree that placing a fault
/// needs. Most verdicts place nothing, so each is built at most once, on demand.
struct Document<'a> {
    text: &'a str,
    /// The byte offset each line starts at, so placing a warning per variant
    /// costs a search rather than a scan of the text.
    line_starts: OnceCell<Vec<usize>>,
    tree: OnceCell<Option<DeTable<'a>>>,
}

impl<'a> Document<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            line_starts: OnceCell::new(),
            tree: OnceCell::new(),
        }
    }

    fn tree(&self) -> Option<&DeTable<'a>> {
        self.tree
            .get_or_init(|| {
                DeTable::parse(self.text)
                    .ok()
                    .map(toml::Spanned::into_inner)
            })
            .as_ref()
    }

    /// The 1-based line and column of byte `at`. The column counts characters,
    /// as toml's own message does, and skips a `\r`.
    fn position(&self, at: usize) -> Option<(usize, usize)> {
        let starts = self.line_starts.get_or_init(|| {
            std::iter::once(0)
                .chain(
                    self.text
                        .match_indices('\n')
                        .map(|(newline, _)| newline + 1),
                )
                .collect()
        });
        let line = starts.partition_point(|&start| start <= at);
        let start = *starts.get(line.checked_sub(1)?)?;
        let column = self
            .text
            .get(start..at)?
            .chars()
            .filter(|c| *c != '\r')
            .count();
        Some((line, column + 1))
    }

    fn line_at(&self, at: usize) -> Option<usize> {
        self.position(at).map(|(line, _)| line)
    }

    /// The line of variant `index`: its `[[variants]]` header, or the opening
    /// brace of its inline table.
    fn variant_line(&self, index: usize) -> Option<usize> {
        let variant = self
            .tree()?
            .get(VARIANTS)?
            .get_ref()
            .as_array()?
            .get(index)?;
        self.line_at(variant.span().start)
    }

    /// The line of the first literal `id` in any `benchmarks`, which may be another
    /// variant's copy of the one at fault.
    fn benchmark_line(&self, id: &str) -> Option<usize> {
        let tree = self.tree()?;
        let variants = tree
            .get(VARIANTS)
            .and_then(|variants| variants.get_ref().as_array())
            .into_iter()
            .flatten()
            .filter_map(|variant| variant.get_ref().as_table());
        let literal = std::iter::once(tree)
            .chain(variants)
            .filter_map(|table| table.get("benchmarks")?.get_ref().as_array())
            .flatten()
            .find(|value| matches!(value.get_ref(), DeValue::String(s) if s == id))?;
        self.line_at(literal.span().start)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use anyhow::Context;
    use rstest::rstest;

    use pipette_core::benchmark::Benchmark;
    use pipette_core::config::{Config, StorageConfig};
    use pipette_plan_types::{BenchmarkId, Sha256};
    use pipette_planner::{CapabilityRuleError, ExpandError, VariantRuleError};
    use pipette_store_fs::build_local_fs_stores;

    use super::*;
    use crate::plan_document::StoredDocumentError;
    use crate::plan_ingestion::ModelCheck;

    const BENCHMARK: &str = "decode_throughput_512_100";

    const MODEL: &str = r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }"#;

    const RUNTIME: &str = r#"{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3" } }"#;

    const SECRET: &str = "hf_SECRET";

    const GGUF: &str = r#"{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }"#;

    const LLAMACPP: &str = r#"{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }"#;

    /// One `[[variants]]` table: `lines`, then `models` and `runtimes`.
    fn variant(lines: &str, model: &str, runtime: &str) -> String {
        format!("\n[[variants]]\n{lines}\nmodels   = [{model}]\nruntimes = [{runtime}]\n")
    }

    /// One MLX variant per entry of `blocks`, each its variant's lines; with
    /// one-line blocks, variant `n` starts on line `3 + 5n`.
    fn document(blocks: &[&str]) -> String {
        let variants: String = blocks
            .iter()
            .map(|lines| variant(lines, MODEL, RUNTIME))
            .collect();
        format!("benchmarks = [\"{BENCHMARK}\"]\n{variants}")
    }

    fn refused(limit: NonZeroUsize, text: &str) -> anyhow::Result<PlanValidation> {
        match stage(limit, pipette_planner::prefix_types(text), text.to_owned())? {
            Stage::Refused(verdict) => Ok(verdict),
            Stage::Expanded { .. } => anyhow::bail!("the document expanded"),
        }
    }

    /// Where the one issue of a refused `text` is placed.
    fn place(text: &str) -> anyhow::Result<(Option<usize>, Option<usize>)> {
        let verdict = refused(NonZeroUsize::MAX, text)?;
        let [issue] = verdict.issues.as_slice() else {
            anyhow::bail!("not one issue: {:?}", verdict.issues);
        };
        Ok((issue.line, issue.column))
    }

    /// [`check`] over an empty store and a catalog of [`BENCHMARK`] alone.
    async fn checked(text: &str) -> anyhow::Result<PlanValidation> {
        let dir = tempfile::tempdir()?;
        let stores = build_local_fs_stores(&Config {
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Config::default()
        })?;
        let definition = "benchmark_type = \"decode_throughput\"\nparameter_prefill_tokens = 512\nparameter_decode_tokens = 100";
        let catalog = HashMap::from([(
            BenchmarkId::try_new(BENCHMARK)?,
            Benchmark::from_toml(BENCHMARK, definition)?,
        )]);
        let ctx = IngestContext::from_stores(
            &stores,
            &catalog,
            ModelCheck::none(),
            NonZeroUsize::MAX,
            Duration::from_secs(3600),
        );
        let Stage::Expanded { text, expansion } = stage(
            NonZeroUsize::MAX,
            pipette_planner::prefix_types(text),
            text.to_owned(),
        )?
        else {
            anyhow::bail!("the document was refused");
        };
        Ok(check(&ctx, text, expansion, Utc::now()).await?)
    }

    #[test]
    fn a_syntax_error_is_placed_where_toml_prints_it() -> anyhow::Result<()> {
        let text = document(&["requires = [\"os:macos\" \"os:ios\"]"]);
        let Err(PlanParseError::Invalid(error)) =
            pipette_planner::parse(&text, &pipette_planner::prefix_types(&text))
        else {
            anyhow::bail!("expected a syntax error");
        };
        let (line, column) = place(&text)?;
        let printed = format!(
            "line {}, column {}",
            line.context("a line")?,
            column.context("a column")?
        );
        assert!(error.to_string().contains(&printed), "{printed} in {error}");
        Ok(())
    }

    /// A fault of the root table could only read as 1:1, and a token outside a
    /// model has no variant, so neither gets a position.
    #[rstest]
    #[case::unknown_key(document(&["requires = [\"os:macos\"]\nmodelz = 1"]), (Some(5), Some(1)))]
    // The column counts characters, past a CRLF and multi-byte text.
    #[case::crlf_and_multibyte(
        "# ünïcödé\r\nbenchmarks = [\"é\" \"x\"]\r\n".to_owned(),
        (Some(2), Some(19))
    )]
    #[case::no_variants(format!("benchmarks = [\"{BENCHMARK}\"]\n"), (None, None))]
    #[case::empty(String::new(), (None, None))]
    #[case::token_in_a_comment(
        format!("# auth_token = \"{SECRET}\"\n{}", document(&["requires = [\"os:macos\"]"])),
        (None, None)
    )]
    // No eligibility in the second `[[variants]]`.
    #[case::table_header(document(&["requires = [\"os:macos\"]", ""]), (Some(8), None))]
    #[case::inline_table(
        format!(
            "benchmarks = [\"{BENCHMARK}\"]\nvariants = [\n  {{ requires = [\"os:macos\"], models = [{MODEL}], runtimes = [{RUNTIME}] }},\n  {{ models = [{MODEL}], runtimes = [{RUNTIME}] }},\n]\n"
        ),
        (Some(4), None)
    )]
    // A token value in the second variant's environment.
    #[case::hf_token_env(
        document(&["requires = [\"os:macos\"]"]) + &variant(
            "requires = [\"os:macos\"]\nruntime_flags = [{ benchmark_type = \"decode_throughput\", runtime_type = \"llamacpp_cli_stock_tools\", model_type = \"gguf_text\", envs = [\"HF_TOKEN=hf_SECRET\"] }]",
            GGUF,
            LLAMACPP,
        ),
        (Some(8), None)
    )]
    // A `not_all_of` group no client carries whole.
    #[case::capability_rule(
        document(&[
            "requires = [\"os:macos\"]",
            "requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\", \"chip:applem2\"]]",
        ]),
        (Some(8), None)
    )]
    fn a_refusal_is_placed_at_its_fault(
        #[case] text: String,
        #[case] at: (Option<usize>, Option<usize>),
    ) -> anyhow::Result<()> {
        assert_eq!(place(&text)?, at);
        Ok(())
    }

    /// A [`DocumentError::Join`], from a task aborted before it ran.
    fn cancelled() -> anyhow::Result<DocumentError> {
        tokio::runtime::Builder::new_current_thread()
            .build()?
            .block_on(async {
                let task = tokio::spawn(std::future::pending::<()>());
                task.abort();
                task.await
                    .err()
                    .map(DocumentError::Join)
                    .context("an aborted task")
            })
    }

    /// An author's fault is one error; a server's fault stays an error.
    #[rstest]
    #[case::invalid(
        pipette_planner::parse("benchmarks = [", &pipette_planner::prefix_types("benchmarks = [")).err().map(DocumentError::Parse).context("a TOML fault"),
        true
    )]
    #[case::hf_token_env(
        Ok(DocumentError::Parse(PlanParseError::HfTokenEnv { variant: 0, name: "HF_TOKEN".to_owned() })),
        true
    )]
    #[case::count(
        Ok(DocumentError::Count(ExpandError::CapabilityRule(VariantRuleError {
            variant: 0,
            runtime: "mlx",
            rule: CapabilityRuleError::OneOfNoCommit { one_of: Vec::new() },
        }))),
        true
    )]
    #[case::expand(
        serde_json::from_str::<Value>("{")
            .err()
            .map(|e| DocumentError::Expand(ExpandError::Render(e)))
            .context("a JSON fault"),
        true
    )]
    #[case::too_many_jobs(Ok(DocumentError::TooManyJobs { count: 2, limit: NonZeroUsize::MIN }), true)]
    #[case::token_outside_a_model(Ok(DocumentError::TokenOutsideAModel), true)]
    #[case::stored(Ok(DocumentError::Stored(StoredDocumentError::Unparsable)), false)]
    #[case::digest(
        Sha256::try_new(String::new()).err().map(DocumentError::Digest).context("a digest fault"),
        false
    )]
    #[case::join(cancelled(), false)]
    fn only_an_authors_fault_is_a_verdict(
        #[case] error: anyhow::Result<DocumentError>,
        #[case] caller_fault: bool,
    ) -> anyhow::Result<()> {
        let severities = refusal("", error?).ok().map(|verdict| {
            verdict
                .issues
                .iter()
                .map(|issue| issue.severity)
                .collect::<Vec<_>>()
        });
        assert_eq!(severities, caller_fault.then(|| vec![IssueSeverity::Error]));
        Ok(())
    }

    #[test]
    fn a_document_over_the_ceiling_has_no_job_count() -> anyhow::Result<()> {
        let text = format!(
            "benchmarks = [\"{BENCHMARK}\", \"end_to_end_latency_512_256\"]\n\n[[variants]]\nrequires = [\"os:macos\"]\nmodels   = [{MODEL}]\nruntimes = [{RUNTIME}]\n"
        );
        let verdict = refused(NonZeroUsize::MIN, &text)?;
        let severities: Vec<IssueSeverity> =
            verdict.issues.iter().map(|issue| issue.severity).collect();
        assert_eq!(
            (verdict.job_count, verdict.gated_model_count, severities),
            (None, None, vec![IssueSeverity::Error])
        );
        Ok(())
    }

    /// An unknown benchmark is placed at its literal, the expansion still
    /// counts its jobs, and the message names no job index.
    #[rstest]
    #[case::root(
        format!("benchmarks = [\n  \"decode_throughput_512_10\",\n]\n\n[[variants]]\nrequires = [\"os:macos\"]\nmodels   = [{MODEL}]\nruntimes = [{RUNTIME}]\n"),
        2
    )]
    #[case::variant(
        document(&[
            "requires = [\"os:macos\"]",
            "requires = [\"os:macos\"]\nbenchmarks = [\"decode_throughput_512_10\"]",
        ]),
        10
    )]
    #[tokio::test]
    async fn an_unknown_benchmark_is_placed_at_its_literal(
        #[case] text: String,
        #[case] line: usize,
    ) -> anyhow::Result<()> {
        let verdict = checked(&text).await?;
        let placed: Vec<(Option<usize>, &str)> = verdict
            .issues
            .iter()
            .map(|issue| (issue.line, issue.message.as_str()))
            .collect();
        assert_eq!(
            (verdict.job_count.is_some(), placed),
            (
                true,
                vec![(
                    Some(line),
                    "spec.benchmark decode_throughput_512_10 not in the benchmark catalog; did \
                     you mean decode_throughput_512_100?"
                )]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_planner_warning_is_placed_at_its_variant() -> anyhow::Result<()> {
        let text = document(&[
            "requires = [\"os:macos\"]",
            "clients = [\"ev1_a\"]\nclient_versions = [\"1.0\"]",
        ]);
        let verdict = checked(&text).await?;
        let placed: Vec<(IssueSeverity, Option<usize>)> = verdict
            .issues
            .iter()
            .map(|issue| (issue.severity, issue.line))
            .collect();
        assert_eq!(placed, [(IssueSeverity::Warning, Some(8))]);
        Ok(())
    }

    /// A gated model counts once however many jobs run it, and the verdict
    /// never holds its token, refused or not.
    #[rstest]
    #[case::valid(BENCHMARK, &[])]
    #[case::unknown_benchmark("decode_throughput_512_10", &[IssueSeverity::Error])]
    #[tokio::test]
    async fn a_gated_model_is_counted_and_its_token_withheld(
        #[case] benchmark: &str,
        #[case] severities: &[IssueSeverity],
    ) -> anyhow::Result<()> {
        let model = GGUF.replace(" }", &format!(", auth_token = \"{SECRET}\" }}"));
        let text = format!(
            "benchmarks = [\"{benchmark}\"]\n{}{}",
            variant("requires = [\"os:macos\"]", &model, LLAMACPP),
            variant("clients = [\"ev1_a\"]", &model, LLAMACPP),
        );
        let verdict = checked(&text).await?;
        let json = serde_json::to_string(&verdict)?;
        let issues: Vec<IssueSeverity> =
            verdict.issues.iter().map(|issue| issue.severity).collect();
        assert_eq!(
            (
                verdict.job_count,
                verdict.gated_model_count,
                issues.as_slice(),
                json.contains(SECRET)
            ),
            (Some(2), Some(1), severities, false),
            "{json}"
        );
        Ok(())
    }
}
