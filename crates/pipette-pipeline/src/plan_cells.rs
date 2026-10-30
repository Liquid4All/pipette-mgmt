//! A plan's cells: each job's [`PlanCell`], its `spec` as a record carries it
//! ([`ClientRunSpec::for_record`]), in the order the plan's document lists
//! them, and the process-local cache that spares a repeat read its round trips.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use pipette_core::plan::PlanSource;
use pipette_plan_types::{
    BenchmarkId, BenchmarkType, ClientRunSpec, JobId, PlanId, benchmark_type_of,
};

/// A job's request: its spec as a record carries it (no HuggingFace token, env
/// values only under `GGML_*`), beside the benchmark type filing gave it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PlanCell {
    #[serde(flatten)]
    spec: ClientRunSpec,
    /// `null` only for an id with no type prefix on a plan filed before its
    /// manifest recorded `benchmark_types`.
    benchmark_type: Option<BenchmarkType>,
}

impl From<&PlanCell> for pipette_plan_types::PlanCell {
    fn from(cell: &PlanCell) -> Self {
        Self {
            spec: cell.spec().clone(),
            benchmark_type: cell.benchmark_type(),
        }
    }
}

impl PlanCell {
    /// `spec` as a record carries it, typed by `types`, its plan's
    /// [`filed_types`].
    pub fn of(spec: &ClientRunSpec, types: &BTreeMap<BenchmarkId, BenchmarkType>) -> Self {
        let spec = spec.for_record();
        Self {
            benchmark_type: types.get(&spec.benchmark).copied(),
            spec,
        }
    }

    pub fn spec(&self) -> &ClientRunSpec {
        &self.spec
    }

    pub fn benchmark_type(&self) -> Option<BenchmarkType> {
        self.benchmark_type
    }
}

/// Each of a plan's jobs and its cell, `None` for a job with no spec.
pub type JobCells = Arc<[(JobId, Option<PlanCell>)]>;

/// The types a plan's cells were filed under: `recorded`, its manifest's
/// `benchmark_types`. A plan filed before the manifest recorded them has none,
/// and was typed by each id's prefix, so that is how it still reads.
pub fn filed_types<'a>(
    recorded: &BTreeMap<BenchmarkId, BenchmarkType>,
    ids: impl IntoIterator<Item = &'a BenchmarkId>,
) -> BTreeMap<BenchmarkId, BenchmarkType> {
    if !recorded.is_empty() {
        return recorded.clone();
    }
    ids.into_iter()
        .filter_map(|id| Some((id.clone(), benchmark_type_of(id)?)))
        .collect()
}

/// The cells `document` lists, in its order (variant, then benchmark, then
/// model), typed by `types`; `None` for one that no longer parses or expands
/// past `max_cells`.
pub fn document_cells(
    document: &str,
    max_cells: usize,
    types: &BTreeMap<BenchmarkId, BenchmarkType>,
) -> Option<Vec<PlanCell>> {
    let plan = pipette_planner::parse(document, types).ok()?;
    if pipette_planner::job_count(&plan).ok()? > max_cells {
        return None;
    }
    let cells = plan.cells().ok()?;
    Some(
        cells
            .iter()
            .map(|cell| PlanCell::of(&cell.spec, types))
            .collect(),
    )
}

/// [`document_cells`] of a stored document, on the blocking pool under the
/// caller's span. The error is the task's panic, a server fault.
pub async fn spawn_document_cells(
    source: PlanSource,
    max_cells: usize,
    types: BTreeMap<BenchmarkId, BenchmarkType>,
) -> Result<Option<Vec<PlanCell>>, tokio::task::JoinError> {
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        span.in_scope(|| document_cells(source.text(), max_cells, &types))
    })
    .await
}

/// `jobs` in the order `document` lists their cells, unlisted ones after. Job
/// order alone is the planner's body sort, which puts `_1024` before `_256`.
pub fn in_document_order(jobs: &mut [(JobId, Option<PlanCell>)], document: &[PlanCell]) {
    let rank: HashMap<&PlanCell, usize> =
        document
            .iter()
            .enumerate()
            .fold(HashMap::new(), |mut rank, (at, cell)| {
                rank.entry(cell).or_insert(at);
                rank
            });
    // Stable, so jobs on one cell, and those on none, keep their order.
    jobs.sort_by_key(|(_, cell)| {
        cell.as_ref()
            .and_then(|cell| rank.get(cell).copied())
            .unwrap_or(usize::MAX)
    });
}

/// The cells [`CellCache`] keeps across all plans. Past it the map starts
/// over: a cold read costs round trips, never a wrong answer.
const CELLS_MAX: usize = 50_000;

/// Each plan's [`JobCells`], kept for the next read. Process-local; a plan's
/// jobs are fixed at ingestion and a job's stored source is written once.
#[derive(Default)]
pub struct CellCache(Mutex<HashMap<PlanId, JobCells>>);

/// A poisoned lock means another reader panicked mid-insert. A map of final
/// values cannot be left torn by that, so its contents stay usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CellCache {
    /// The cells kept for `plan_id`, if they were read for its `jobs` jobs.
    pub fn get(&self, plan_id: &PlanId, jobs: usize) -> Option<JobCells> {
        lock(&self.0)
            .get(plan_id)
            .filter(|cells| cells.len() == jobs)
            .cloned()
    }

    /// Keep `plan_id`'s cells. Only those of a plan past ingestion are final:
    /// until then a job's source may yet be written.
    pub fn keep(&self, plan_id: &PlanId, cells: &JobCells) {
        if cells.len() > CELLS_MAX {
            return;
        }
        let mut plans = lock(&self.0);
        plans.remove(plan_id);
        if plans.values().map(|kept| kept.len()).sum::<usize>() + cells.len() > CELLS_MAX {
            plans.clear();
        }
        plans.insert(plan_id.clone(), cells.clone());
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use serde::Deserialize;
    use serde_json::{Value, json};

    use pipette_plan_types::ClientRunSpec;
    use pipette_planner::prefix_types;

    use super::*;

    const RUNTIME: &str = r#"{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }"#;

    /// A plan over one GGUF model per repo of `repos`, its variant's `extra`
    /// lines (eligibility, flags) after it.
    fn document(benchmarks: &[&str], repos: &[&str], extra: &str) -> String {
        let models = repos
            .iter()
            .map(|repo| {
                format!(
                    r#"{{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "{repo}", path = "a.gguf" }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "benchmarks = {benchmarks:?}\n\n[[variants]]\nrequires = [\"os:macos\"]\nmodels = [{models}]\nruntimes = [{RUNTIME}]\n{extra}\n"
        )
    }

    fn specs(document: &str) -> anyhow::Result<Vec<ClientRunSpec>> {
        pipette_planner::expand(&pipette_planner::parse(document, &prefix_types(document))?)?
            .bodies
            .iter()
            .map(|body| {
                let spec = body.get("spec").context("a job body holds its spec")?;
                Ok(ClientRunSpec::deserialize(spec)?)
            })
            .collect()
    }

    fn spec_of<'a>(
        specs: &'a [ClientRunSpec],
        benchmark: &str,
        repo: &str,
    ) -> anyhow::Result<&'a ClientRunSpec> {
        specs
            .iter()
            .find(|spec| {
                spec.benchmark.as_ref() == benchmark
                    && serde_json::to_value(&spec.model)
                        .is_ok_and(|model| model.get("repo_name") == Some(&json!(repo)))
            })
            .with_context(|| format!("a job runs {benchmark} on {repo}"))
    }

    fn only_spec(document: &str) -> anyhow::Result<ClientRunSpec> {
        let [spec] = specs(document)?
            .try_into()
            .map_err(|specs| anyhow::anyhow!("one job, got {specs:?}"))?;
        Ok(spec)
    }

    fn prefill_flags(knobs: &str) -> String {
        format!(
            r#"runtime_flags = [{{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", {knobs} }}]"#
        )
    }

    /// `spec`'s cell as a plan filed before `benchmark_types` reads it.
    fn prefixed(spec: &ClientRunSpec) -> PlanCell {
        PlanCell::of(spec, &filed_types(&BTreeMap::new(), [&spec.benchmark]))
    }

    fn job(n: u128) -> JobId {
        JobId::from_uuid(uuid::Uuid::from_u128(n))
    }

    fn plan(n: u128) -> PlanId {
        PlanId::from_uuid(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn a_cell_reads_as_its_jobs_spec() -> anyhow::Result<()> {
        let spec = only_spec(&document(
            &["prefill_throughput_512"],
            &["a"],
            &prefill_flags("threads = 4"),
        ))?;
        let mut read = serde_json::to_value(prefixed(&spec))?;
        let fields = read.as_object_mut().context("a cell is an object")?;
        assert_eq!(
            fields.remove("benchmark_type"),
            Some(json!("prefill_throughput"))
        );
        assert_eq!(read, serde_json::to_value(&spec)?);
        Ok(())
    }

    /// Two jobs that differ in a hidden env value alone read alike.
    #[test]
    fn flags_keep_their_axes_and_redact_env_values() -> anyhow::Result<()> {
        let flagged = |key: &str| -> anyhow::Result<PlanCell> {
            let spec = only_spec(&document(
                &["prefill_throughput_512"],
                &["a"],
                &prefill_flags(&format!(r#"envs = ["LLAMA_API_KEY={key}", "GGML_X=1"]"#)),
            ))?;
            Ok(prefixed(&spec))
        };
        let cell = flagged("sk-secret")?;
        assert_eq!(cell, flagged("sk-other")?);
        assert_eq!(
            serde_json::to_value(&cell)?["runtime_flags"],
            json!({
                "benchmark_type": "prefill_throughput",
                "runtime_type": "llamacpp_cli_stock_tools",
                "model_type": "gguf_text",
                "envs": ["LLAMA_API_KEY", "GGML_X=1"],
            })
        );
        assert!(!serde_json::to_string(&cell)?.contains("sk-"));
        Ok(())
    }

    #[test]
    fn an_untyped_id_has_no_type_and_no_flags() -> anyhow::Result<()> {
        let specs = specs(&document(
            &["prefill_throughput_512", "custom_probe"],
            &["a"],
            &prefill_flags("threads = 4"),
        ))?;
        let typed = spec_of(&specs, "prefill_throughput_512", "a")?;
        let untyped = spec_of(&specs, "custom_probe", "a")?;
        assert!(prefixed(typed).spec().runtime_flags.is_some());
        let untyped = serde_json::to_value(prefixed(untyped))?;
        assert_eq!(untyped.get("benchmark_type"), Some(&Value::Null));
        assert_eq!(untyped.get("benchmark"), Some(&json!("custom_probe")));
        ["model_flags", "runtime_flags", "benchmark_flags"]
            .into_iter()
            .for_each(|group| {
                assert_eq!(
                    untyped.get(group),
                    Some(&Value::Null),
                    "{group} in {untyped}"
                );
            });
        Ok(())
    }

    /// A job the document does not list goes after, and so does one with no cell,
    /// each in its own order.
    #[test]
    fn the_document_orders_the_jobs() -> anyhow::Result<()> {
        let document = document(
            &["prefill_throughput_512", "decode_throughput_512_100"],
            &["b", "a"],
            "",
        );
        let planned = specs(&document)?;
        let cell = |benchmark: &str, repo: &str| -> anyhow::Result<PlanCell> {
            Ok(prefixed(spec_of(&planned, benchmark, repo)?))
        };
        let probe = only_spec(&self::document(&["custom_probe"], &["c"], ""))?;
        let mut jobs = vec![
            (job(1), Some(cell("decode_throughput_512_100", "a")?)),
            (job(2), None),
            (job(3), Some(prefixed(&probe))),
            (job(4), Some(cell("prefill_throughput_512", "a")?)),
            (job(5), Some(cell("decode_throughput_512_100", "b")?)),
            (job(6), Some(cell("prefill_throughput_512", "b")?)),
        ];
        let order = document_cells(&document, usize::MAX, &prefix_types(&document))
            .context("the document reads")?;
        in_document_order(&mut jobs, &order);
        let listed: Vec<JobId> = jobs.into_iter().map(|(job_id, _)| job_id).collect();
        assert_eq!(listed, [job(6), job(4), job(5), job(1), job(2), job(3)]);
        Ok(())
    }

    #[test]
    fn an_unprefixed_id_reads_as_filed() -> anyhow::Result<()> {
        let document = document(
            &["acme_long_prompt", "prefill_throughput_512"],
            &["a"],
            &prefill_flags("threads = 4"),
        );
        let types = BTreeMap::from([
            (
                BenchmarkId::try_new("acme_long_prompt")?,
                BenchmarkType::PrefillThroughput,
            ),
            (
                BenchmarkId::try_new("prefill_throughput_512")?,
                BenchmarkType::PrefillThroughput,
            ),
        ]);
        let filed = pipette_planner::parse(&document, &types)?;
        let cells: Vec<PlanCell> = filed
            .cells()?
            .iter()
            .map(|cell| PlanCell::of(&cell.spec, &types))
            .collect();
        let [private, public] = cells.as_slice() else {
            anyhow::bail!("two cells, got {cells:?}");
        };
        assert!(private.spec().runtime_flags.is_some());
        assert_eq!(
            private.benchmark_type(),
            Some(BenchmarkType::PrefillThroughput)
        );
        let mut jobs = vec![
            (job(1), Some(public.clone())),
            (job(2), Some(private.clone())),
        ];
        let order = document_cells(&document, usize::MAX, &types).context("the document reads")?;
        in_document_order(&mut jobs, &order);
        let listed: Vec<JobId> = jobs.into_iter().map(|(job_id, _)| job_id).collect();
        assert_eq!(listed, [job(2), job(1)]);
        Ok(())
    }

    /// A private id whose prefix names another type reads with the type filing
    /// recorded, flagged or not.
    #[test]
    fn a_misleading_prefix_reads_as_filed() -> anyhow::Result<()> {
        let id = BenchmarkId::try_new("decode_throughput_acme")?;
        let document = document(&[id.as_ref()], &["a"], &prefill_flags("threads = 4"));
        let types = BTreeMap::from([(id.clone(), BenchmarkType::PrefillThroughput)]);
        let filed = pipette_planner::parse(&document, &types)?;
        let [flagged] = filed
            .cells()?
            .iter()
            .map(|cell| PlanCell::of(&cell.spec, &types))
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|cells| anyhow::anyhow!("one cell, got {cells:?}"))?;
        assert!(flagged.spec().runtime_flags.is_some());
        let mut flagless_spec = flagged.spec().clone();
        flagless_spec.runtime_flags = None;
        let flagless = PlanCell::of(&flagless_spec, &types);
        assert_eq!(
            flagless.benchmark_type(),
            Some(BenchmarkType::PrefillThroughput)
        );
        let order = document_cells(&document, usize::MAX, &types).context("the document reads")?;
        assert_eq!(order, [flagged]);
        Ok(())
    }

    /// A plan's recorded types stand; one filed before it recorded any reads by
    /// prefix, as it was filed.
    #[test]
    fn a_plan_reads_by_the_types_it_was_filed_under() -> anyhow::Result<()> {
        let probe = BenchmarkId::try_new("custom_probe")?;
        let prefill = BenchmarkId::try_new("prefill_throughput_512")?;
        let recorded = BTreeMap::from([(probe.clone(), BenchmarkType::Eval)]);
        assert_eq!(filed_types(&recorded, [&probe, &prefill]), recorded);
        assert_eq!(
            filed_types(&BTreeMap::new(), [&probe, &prefill]),
            BTreeMap::from([(prefill, BenchmarkType::PrefillThroughput)])
        );
        Ok(())
    }

    #[test]
    fn a_document_that_does_not_read_orders_nothing() {
        let document = document(&["prefill_throughput_512"], &["a", "b"], "");
        let types = prefix_types(&document);
        assert_eq!(
            document_cells(&document, 2, &types).map(|cells| cells.len()),
            Some(2)
        );
        assert_eq!(document_cells(&document, 1, &types), None);
        assert_eq!(document_cells("benchmarks = [", usize::MAX, &types), None);
    }

    /// An older build's job can carry a model token; its cell drops it.
    #[test]
    fn a_cell_has_no_auth_token() -> anyhow::Result<()> {
        let gated = r#"benchmarks = ["prefill_throughput_512"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "gated", path = "a.gguf", auth_token = "hf_supersecret" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
"#;
        let plan = pipette_planner::parse(gated, &prefix_types(gated))?;
        let cells = plan.cells()?;
        let [cell] = cells.as_slice() else {
            anyhow::bail!("one cell, got {cells:?}");
        };
        anyhow::ensure!(cell.spec.model.auth_token().is_some(), "the spec holds it");
        let body = serde_json::to_string(&prefixed(&cell.spec))?;
        assert!(body.contains("\"repo_name\":\"gated\""), "{body}");
        assert!(!body.contains("auth_token"), "{body}");
        assert!(!body.contains("hf_supersecret"), "{body}");
        Ok(())
    }

    /// Kept cells answer only for the job count they were read over.
    #[test]
    fn kept_cells_answer_for_their_job_count() -> anyhow::Result<()> {
        let cell = prefixed(&only_spec(&document(
            &["prefill_throughput_512"],
            &["a"],
            "",
        ))?);
        let cache = CellCache::default();
        let cells: JobCells = Arc::from(vec![(job(1), Some(cell)), (job(2), None)]);
        cache.keep(&plan(1), &cells);
        assert_eq!(cache.get(&plan(1), 2), Some(cells));
        assert!(cache.get(&plan(1), 3).is_none());
        assert!(cache.get(&plan(2), 2).is_none());
        Ok(())
    }

    /// Past its bound in cells the map starts over, and a plan past it alone
    /// is never kept.
    #[test]
    fn the_cache_starts_over_past_its_bound() -> anyhow::Result<()> {
        let cell = prefixed(&only_spec(&document(
            &["prefill_throughput_512"],
            &["a"],
            "",
        ))?);
        let cells = |n: usize| -> JobCells {
            (0..n as u128)
                .map(|at| (job(at), Some(cell.clone())))
                .collect()
        };
        let half = cells(CELLS_MAX / 2);
        let cache = CellCache::default();
        cache.keep(&plan(1), &half);
        cache.keep(&plan(2), &half);
        assert!(cache.get(&plan(1), half.len()).is_some());

        cache.keep(&plan(3), &cells(1));
        assert!(cache.get(&plan(1), half.len()).is_none());
        assert!(cache.get(&plan(3), 1).is_some());

        cache.keep(&plan(4), &cells(CELLS_MAX + 1));
        assert!(cache.get(&plan(4), CELLS_MAX + 1).is_none());
        assert!(cache.get(&plan(3), 1).is_some());
        Ok(())
    }
}
