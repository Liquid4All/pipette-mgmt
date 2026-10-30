//! The import step of ingestion: each model carrying an `auth_token` is
//! imported into the vault and its body rewritten to the vault arm, so no
//! staged body holds a token. Every refusal comes before any import starts.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

use pipette_core::vault::VaultOwner;
use pipette_plan_types::{Model, PlannedImport};
use pipette_store::VaultStore;

use crate::hf_import::{HfImports, ImportFault, ImportPlan, Importer, ModelImportError};
use crate::plan_ingestion::{ImportStepError, IngestError, JobRejectionError, RejectionError};
use crate::submission::without_auth_tokens;

const AUTH_TOKEN: &str = "auth_token";

const MODEL_PREFIX: &str = "/spec/model/";

/// One job after the import step.
#[derive(Debug)]
pub(crate) struct GatedJob {
    pub(crate) label: String,
    /// The body, with an imported model rewritten to its vault arm.
    pub(crate) body: Value,
    /// The replaced HuggingFace model, without its token, which the table check
    /// and the recipe check read. Until `prepare_jobs` fills the rest, `Some`
    /// marks the rewritten jobs.
    pub(crate) hf_model: Option<Value>,
}

/// The job set after the import step.
#[derive(Debug)]
pub(crate) struct Gated {
    pub(crate) jobs: Vec<GatedJob>,
    /// Each imported model, without its token, mapped to its vault arm. Models
    /// that differ only in the token share one import.
    pub(crate) rewrites: HashMap<Model, Model>,
}

/// The job set after the import step resolved its models, in a dry run.
#[derive(Debug)]
pub(crate) struct DryRunImports {
    /// The jobs as a real ingest stages them, with [`Unresolved::Pending`]
    /// placeholders.
    pub(crate) jobs: Vec<GatedJob>,
    /// What the import does for each distinct model.
    pub(crate) entries: Vec<PlannedImport>,
}

/// The importable model of each job, after the refusals of the import step.
struct Found<'a> {
    /// The model that the import takes from each job, by index.
    models: Vec<Option<Model>>,
    imports: &'a HfImports,
}

/// Find each model the import takes, and refuse a job set it cannot serve.
/// `None` when no job carries a token.
fn find<'a>(
    imports: Option<&'a HfImports>,
    jobs: &[(String, Value)],
) -> Result<Option<Found<'a>>, IngestError> {
    let models: Vec<Option<Model>> = jobs.iter().map(|(_, body)| importable(body)).collect();
    if let Some((index, field)) =
        jobs.iter()
            .zip(&models)
            .enumerate()
            .find_map(|(index, ((_, body), model))| {
                let left = token_pointers(body)
                    .into_iter()
                    .find(|field| model.is_none() || !field.starts_with(MODEL_PREFIX))?;
                Some((index, left))
            })
    {
        // A refusal names the token by JSON pointer, never its value.
        tracing::warn!(
            job_index = index,
            field = %field,
            "rejected a job body: a token that the import does not remove"
        );
        return Err(refuse(jobs, index, JobRejectionError::AuthToken { field }));
    }
    let Some(first) = models.iter().position(Option::is_some) else {
        return Ok(None);
    };
    let imports =
        imports.ok_or_else(|| refuse(jobs, first, JobRejectionError::ImportUnavailable))?;
    Ok(Some(Found { models, imports }))
}

/// The jobs of a set in which no job carries a token, each with its body.
fn untouched(jobs: Vec<(String, Value)>) -> Vec<GatedJob> {
    jobs.into_iter()
        .map(|(label, body)| GatedJob {
            label,
            body,
            hf_model: None,
        })
        .collect()
}

/// The rejection of job `index` of `jobs` for `kind`.
fn refuse(jobs: &[(String, Value)], index: usize, kind: JobRejectionError) -> IngestError {
    let label = jobs
        .get(index)
        .map(|(label, _)| label.clone())
        .unwrap_or_default();
    IngestError::Rejected(RejectionError::Job { index, label, kind })
}

/// Import each model that carries a token, for `owner`, and rewrite its body.
pub(crate) async fn import(
    imports: Option<&HfImports>,
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    jobs: Vec<(String, Value)>,
    now: DateTime<Utc>,
) -> Result<Gated, IngestError> {
    let Some(Found { models, imports }) = find(imports, &jobs)? else {
        return Ok(Gated {
            jobs: untouched(jobs),
            rewrites: HashMap::new(),
        });
    };
    let rewrites: HashMap<Model, Model> = run(imports, vault, owner, &jobs, &models, now)
        .await?
        .into_iter()
        .map(|(model, arm)| (model.without_auth_token(), arm))
        .collect();
    // A body that keeps its model would stage the token.
    if let Some(model) = models
        .iter()
        .flatten()
        .find(|model| !rewrites.contains_key(&model.without_auth_token()))
    {
        return Err(ImportStepError::NoVaultArm {
            model: model.to_string(),
        }
        .into());
    }
    let jobs = rewrite(jobs, &models, &rewrites, Unresolved::Keep)?;
    tracing::info!(
        rewritten = jobs.iter().filter(|job| job.hf_model.is_some()).count(),
        "hf import step rewrote job bodies"
    );
    Ok(Gated { jobs, rewrites })
}

/// Apply [`import`]'s refusals and resolve each tokened model against the Hub.
/// Downloads nothing, writes nothing, and takes no import permit.
pub(crate) async fn dry_run(
    imports: Option<&HfImports>,
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    jobs: Vec<(String, Value)>,
) -> Result<DryRunImports, IngestError> {
    let Some(Found { models, imports }) = find(imports, &jobs)? else {
        return Ok(DryRunImports {
            jobs: untouched(jobs),
            entries: Vec::new(),
        });
    };
    let importer = imports.importer(vault);
    let plan = resolve(&importer, owner, &jobs, &models).await?;
    let arms = plan
        .reused_arms()
        .map_err(|failure| failure_of(failure, &jobs, &models))?;
    let jobs = rewrite(jobs, &models, &arms, Unresolved::Pending)?;
    Ok(DryRunImports {
        jobs,
        entries: plan.entries(),
    })
}

/// What [`rewrite`] does with a job whose model has no vault arm.
#[derive(Debug, Clone, Copy)]
enum Unresolved {
    Keep,
    /// The job gets `{"pending_import": <model without token>}`, which matches no
    /// stored body, so a revision preview counts it as a real filing would.
    Pending,
}

/// Give each job whose model is in `arms` that vault arm; one in `models` but
/// not in `arms` is `unresolved`.
fn rewrite(
    jobs: Vec<(String, Value)>,
    models: &[Option<Model>],
    arms: &HashMap<Model, Model>,
    unresolved: Unresolved,
) -> Result<Vec<GatedJob>, ImportStepError> {
    jobs.into_iter()
        .zip(models)
        .map(|((label, mut body), model)| {
            let Some(model) = model.as_ref().map(Model::without_auth_token) else {
                return Ok(GatedJob {
                    label,
                    body,
                    hf_model: None,
                });
            };
            let (replacement, imported) = match (arms.get(&model), unresolved) {
                (Some(vault_arm), _) => (model_json(&label, vault_arm)?, true),
                (None, Unresolved::Pending) => {
                    let pending = Value::Object(serde_json::Map::from_iter([(
                        "pending_import".to_string(),
                        model_json(&label, &model)?,
                    )]));
                    (pending, false)
                }
                (None, Unresolved::Keep) => {
                    return Ok(GatedJob {
                        label,
                        body,
                        hf_model: None,
                    });
                }
            };
            let Some(slot) = body.get_mut("spec").and_then(|spec| spec.get_mut("model")) else {
                return Err(ImportStepError::LostModel { label });
            };
            let hf_model = imported.then(|| without_auth_tokens(slot));
            *slot = replacement;
            Ok(GatedJob {
                label,
                body,
                hf_model,
            })
        })
        .collect()
}

/// `model` as JSON, for the body of the job `label`.
fn model_json(label: &str, model: &Model) -> Result<Value, ImportStepError> {
    serde_json::to_value(model).map_err(|source| ImportStepError::ModelJson {
        label: label.to_owned(),
        source,
    })
}

/// Resolve every model, then fetch them, holding one process permit if any
/// download. Jobs naming one commit with different tokens share one import.
async fn run(
    imports: &HfImports,
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    jobs: &[(String, Value)],
    found: &[Option<Model>],
    now: DateTime<Utc>,
) -> Result<HashMap<Model, Model>, IngestError> {
    let models = found.iter().flatten().count();
    let distinct = found.iter().flatten().collect::<HashSet<_>>().len();
    tracing::info!(
        owner = %owner.label(),
        job_count = models,
        models = distinct,
        "hf import step started"
    );
    let importer = imports.importer(vault);
    let plan = resolve(&importer, owner, jobs, found).await?;
    // One permit covers the whole plan; only `vault_blob_limit_bytes` caps each
    // blob, and nothing caps an ingest's total download.
    let _permit = if plan.downloads() {
        let Ok(permit) = imports.permits.try_acquire() else {
            tracing::warn!("hf import refused: every import permit is held");
            return Err(IngestError::ImportBusy);
        };
        Some(permit)
    } else {
        None
    };
    plan.fetch(&importer, &imports.progress, now)
        .await
        .map_err(|failure| failure_of(failure, jobs, found))
}

/// Resolve each model of `found` against the Hub, for `owner`. A failure names
/// the first job that holds its model.
async fn resolve(
    importer: &Importer<'_>,
    owner: &VaultOwner,
    jobs: &[(String, Value)],
    found: &[Option<Model>],
) -> Result<ImportPlan, IngestError> {
    let models: Vec<Model> = found.iter().flatten().cloned().collect();
    ImportPlan::resolve(importer, owner, &models)
        .await
        .map_err(|failure| failure_of(failure, jobs, found))
}

/// The ingest error for `failure`, naming the first job that holds its model.
fn failure_of(
    failure: ModelImportError,
    jobs: &[(String, Value)],
    found: &[Option<Model>],
) -> IngestError {
    let index = found
        .iter()
        .position(|model| model.as_ref() == Some(&*failure.model))
        .unwrap_or_default();
    let label = jobs
        .get(index)
        .map(|(label, _)| label.clone())
        .unwrap_or_default();
    let error = failure.error;
    match error.fault() {
        ImportFault::Caller => IngestError::Rejected(RejectionError::Job {
            index,
            label,
            kind: JobRejectionError::Import {
                reason: error.to_string(),
            },
        }),
        ImportFault::Upstream => IngestError::Upstream {
            index,
            label,
            reason: error.to_string(),
        },
        ImportFault::RateLimited => IngestError::HubRateLimited {
            index,
            label,
            reason: error.to_string(),
        },
        ImportFault::Server => IngestError::Other(
            anyhow::Error::new(error).context("failed to import a HuggingFace model"),
        ),
    }
}

/// The typed `spec.model` of `body`, if it holds a token and round-trips
/// without it: the rewrite would drop a field the type does not know.
fn importable(body: &Value) -> Option<Model> {
    let model = body.get("spec")?.get("model")?;
    if token_pointers(model).is_empty() {
        return None;
    }
    let typed: Model = serde_json::from_value(model.clone()).ok()?;
    let round_trip = serde_json::to_value(typed.without_auth_token()).ok()?;
    (without_nulls(&round_trip) == without_nulls(&without_auth_tokens(model))).then_some(typed)
}

/// The JSON pointer of each `auth_token` in `value` that holds a value, in
/// document order.
pub(crate) fn token_pointers(value: &Value) -> Vec<String> {
    let mut pointers = Vec::new();
    collect(value, &mut Vec::new(), &mut pointers);
    pointers
}

/// One reference token of the path to a value.
enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

/// Walk `value` below the path `at`. The walk formats a pointer only for a
/// token that it finds.
fn collect<'a>(value: &'a Value, at: &mut Vec<Segment<'a>>, pointers: &mut Vec<String>) {
    match value {
        Value::Object(fields) => fields.iter().for_each(|(key, value)| {
            at.push(Segment::Key(key));
            if key == AUTH_TOKEN && !value.is_null() {
                pointers.push(pointer(at));
            }
            collect(value, at, pointers);
            at.pop();
        }),
        Value::Array(items) => items.iter().enumerate().for_each(|(index, item)| {
            at.push(Segment::Index(index));
            collect(item, at, pointers);
            at.pop();
        }),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// The JSON pointer of the path `at`. A key escapes `~` as `~0` and `/` as
/// `~1`.
fn pointer(at: &[Segment<'_>]) -> String {
    at.iter()
        .map(|segment| match segment {
            Segment::Key(key) => format!("/{}", key.replace('~', "~0").replace('/', "~1")),
            Segment::Index(index) => format!("/{index}"),
        })
        .collect()
}

/// `value` with every object entry that holds `null` removed. A typed model
/// omits an absent optional field, and a body may spell it `null`.
pub(crate) fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            scalar.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn gguf(extra: Value) -> Value {
        let mut model = json!({
            "type": "gguf_text", "source": "huggingface", "org": "LiquidAI",
            "repo_name": "LFM2-GGUF", "path": "m.gguf", "auth_token": "hf_SECRET",
        });
        if let (Some(model), Value::Object(extra)) = (model.as_object_mut(), extra) {
            model.extend(extra);
        }
        model
    }

    #[rstest]
    #[case::none(json!({"spec": {"model": {"path": "m"}}}), &[])]
    #[case::model(json!({"spec": {"model": {"auth_token": "x"}}}), &["/spec/model/auth_token"])]
    #[case::nested(json!({"spec": {"runtime": {"source": {"auth_token": "x"}}}}), &["/spec/runtime/source/auth_token"])]
    #[case::in_array(json!({"spec": {"runtime": {"extra": [{"a": 1}, {"auth_token": "x"}]}}}), &["/spec/runtime/extra/1/auth_token"])]
    #[case::escaped(json!({"a/b": {"c~d": {"auth_token": "x"}}}), &["/a~1b/c~0d/auth_token"])]
    #[case::null_is_absent(json!({"spec": {"model": {"auth_token": null}}}), &[])]
    #[case::two(json!({"auth_token": "x", "spec": {"auth_token": "y"}}), &["/auth_token", "/spec/auth_token"])]
    fn every_token_is_found(#[case] body: Value, #[case] expected: &[&str]) {
        assert_eq!(token_pointers(&body), expected);
    }

    /// Each case gives the job the refusal names and, for a token, its field;
    /// `None` is a set [`find`] accepts.
    #[rstest]
    #[case::no_token(json!({"spec": {"model": {"path": "m"}}}), None)]
    #[case::gated_model(json!({"spec": {"model": gguf(json!({}))}}), Some((1, None)))]
    #[case::runtime_token(
        json!({"spec": {"model": gguf(json!({})), "runtime": {"auth_token": "hf_SECRET"}}}),
        Some((1, Some("/spec/runtime/auth_token")))
    )]
    #[case::unknown_model_field(
        json!({"spec": {"model": gguf(json!({"requires_auth": true}))}}),
        Some((1, Some("/spec/model/auth_token")))
    )]
    fn find_refuses_before_the_hub(
        #[case] body: Value,
        #[case] expected: Option<(usize, Option<&str>)>,
    ) -> anyhow::Result<()> {
        let jobs = vec![
            ("0".to_owned(), json!({"spec": {"model": {"path": "m"}}})),
            ("1".to_owned(), body),
        ];
        let refused = match find(None, &jobs) {
            Ok(found) => {
                anyhow::ensure!(found.is_none());
                None
            }
            Err(error) => {
                let IngestError::Rejected(RejectionError::Job { index, kind, .. }) = &error else {
                    anyhow::bail!("an unexpected error: {error}");
                };
                let field = if let JobRejectionError::AuthToken { field } = kind {
                    Some(field.clone())
                } else {
                    anyhow::ensure!(
                        matches!(kind, JobRejectionError::ImportUnavailable),
                        "an unexpected refusal: {kind}"
                    );
                    None
                };
                Some((*index, field))
            }
        };
        assert_eq!(
            refused,
            expected.map(|(index, field)| (index, field.map(str::to_owned)))
        );
        Ok(())
    }

    /// The import takes a model that round-trips, and no other.
    #[rstest]
    #[case::gated(gguf(json!({})), true)]
    #[case::explicit_null(gguf(json!({"revision": null})), true)]
    #[case::unknown_field(gguf(json!({"requires_auth": true})), false)]
    #[case::token_in_unknown_field(gguf(json!({"extra": {"auth_token": "hf_SECRET"}})), false)]
    #[case::no_token(json!({"type": "mlx", "source": "huggingface", "org": "a", "repo_name": "b"}), false)]
    #[case::unparseable(json!({"type": "gguf_text", "path": "m.gguf", "auth_token": "hf_SECRET"}), false)]
    fn only_a_model_that_round_trips_imports(#[case] model: Value, #[case] imports: bool) {
        let body = json!({"spec": {"model": model}});
        assert_eq!(importable(&body).is_some(), imports, "{body}");
    }
}
