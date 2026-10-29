//! The process-local cache of the results of `complete` plans, which
//! `GET /api/v1/plans/{plan_id}/jobs?results=true` reads. It keeps the part
//! of the answer that does not depend on the caller.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::OnceCell;
use tokio::time::Instant;

use pipette_pipeline::plan_cells::JobCells;
use pipette_pipeline::plan_results::ResultRow;
use pipette_plan_types::{JobId, PlanId, ResultsRead};

use super::reads::History;

/// How long the cache keeps a plan after its fill. A rescore or a
/// `fix-model-param` run shows in an answer after this time at most.
const KEEP_FOR: Duration = Duration::from_secs(5 * 60);

/// The most result rows that the cache holds over all plans.
const ROWS_MAX: usize = 100_000;

/// The most completed plans that the cache keeps, including plans with no rows.
const PLANS_MAX: usize = 1_024;

/// The part of a plan's jobs with results that does not depend on the caller.
#[derive(Clone)]
pub(super) struct PlanJobResults {
    /// The plan's jobs and their cells, in plan order.
    pub(super) cells: JobCells,
    /// How the runs were read.
    pub(super) read: ResultsRead,
    /// The rows of each run, by job, at the full tier.
    pub(super) runs: HashMap<JobId, Vec<ResultRow>>,
    pub(super) history: History,
}

impl PlanJobResults {
    /// The result rows of the runs and of the history.
    fn rows(&self) -> usize {
        self.runs.values().map(Vec::len).sum::<usize>() + self.history.rows()
    }

    /// Whether every read came back. The cache keeps only such results.
    fn whole(&self) -> bool {
        self.read == ResultsRead::Ok && self.history.whole
    }
}

/// A plan's results and the time of their fill.
type Filled = (Instant, Arc<PlanJobResults>);

/// One plan's entry. Concurrent reads of the plan wait on one fill of it.
type Slot = Arc<OnceCell<Filled>>;

/// The results of `complete` plans, kept for [`KEEP_FOR`] after their fill
/// and bounded by [`ROWS_MAX`] rows and [`PLANS_MAX`] completed plans.
/// Process-local.
#[derive(Default)]
pub struct PlanResultsCache(Mutex<HashMap<PlanId, Slot>>);

/// A poisoned lock means another reader panicked mid-update. Each update
/// replaces or removes a whole entry, so the map stays usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Remove stale fills and slots whose only owner is the cache after cancellation.
fn prune_slots(slots: &mut HashMap<PlanId, Slot>) {
    slots.retain(|_, slot| match slot.get() {
        Some((filled_at, _)) => fresh(*filled_at),
        None => Arc::strong_count(slot) > 1,
    });
}

/// Whether `filled_at` is within [`KEEP_FOR`].
fn fresh(filled_at: Instant) -> bool {
    filled_at.elapsed() < KEEP_FOR
}

/// Log a read that the cache answered.
fn served(plan_id: &PlanId, filled_at: Instant, window: &'static str) {
    tracing::debug!(
        plan_id = %plan_id,
        age_ms = u64::try_from(filled_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        window,
        "plan results served from the cache"
    );
}

impl PlanResultsCache {
    /// The kept results of `plan_id`, if a fill of them is fresh. A read of a
    /// page uses this, and never fills the cache.
    pub(super) fn get(&self, plan_id: &PlanId) -> Option<Arc<PlanJobResults>> {
        let (filled_at, results) = lock(&self.0).get(plan_id)?.get()?.clone();
        if !fresh(filled_at) || !results.whole() {
            return None;
        }
        served(plan_id, filled_at, "page");
        Some(results)
    }

    /// The kept results of `plan_id`, or `fill`'s. Concurrent calls for one
    /// plan wait for one fill, and all get its answer. The cache does not keep
    /// a fill that failed, that is not whole, or that alone holds more than
    /// [`ROWS_MAX`] rows, so the next call fills again.
    pub(super) async fn get_or_fill<F, Fut, E>(
        &self,
        plan_id: &PlanId,
        fill: F,
    ) -> Result<Arc<PlanJobResults>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<PlanJobResults, E>>,
    {
        let slot = self.slot(plan_id);
        // Set only if this call ran the fill.
        let mut took = None;
        let took_here = &mut took;
        let filled = slot
            .get_or_try_init(move || async move {
                let started = Instant::now();
                let results = fill().await?;
                *took_here = Some(started.elapsed());
                Ok((Instant::now(), Arc::new(results)))
            })
            .await;
        let (filled_at, results) = match filled {
            Ok(filled) => filled.clone(),
            Err(e) => {
                self.forget(plan_id, &slot);
                return Err(e);
            }
        };
        let Some(took) = took else {
            // A concurrent fill that was not whole logged itself.
            if results.whole() {
                served(plan_id, filled_at, "all");
            }
            return Ok(results);
        };
        let rows = results.rows();
        if !results.whole() {
            self.forget(plan_id, &slot);
            tracing::debug!(
                plan_id = %plan_id,
                results_read = ?results.read,
                history_whole = results.history.whole,
                "plan results not cached: a read did not come back"
            );
        } else if rows > ROWS_MAX {
            self.forget(plan_id, &slot);
            tracing::debug!(
                plan_id = %plan_id,
                rows,
                rows_max = ROWS_MAX,
                "plan results not cached: the plan alone is above the row bound"
            );
        } else {
            tracing::info!(
                plan_id = %plan_id,
                elapsed_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                rows,
                jobs = results.cells.len(),
                "plan results cached"
            );
            self.evict_past_bound(plan_id);
        }
        Ok(results)
    }

    /// `plan_id`'s slot. A new slot replaces one whose fill is stale.
    fn slot(&self, plan_id: &PlanId) -> Slot {
        let mut slots = lock(&self.0);
        prune_slots(&mut slots);
        slots.entry(plan_id.clone()).or_default().clone()
    }

    /// Remove `plan_id`'s slot if it is still `slot`. A later call can put a
    /// new slot in its place.
    fn forget(&self, plan_id: &PlanId, slot: &Slot) {
        let mut slots = lock(&self.0);
        if slots
            .get(plan_id)
            .is_some_and(|kept| Arc::ptr_eq(kept, slot))
        {
            slots.remove(plan_id);
        }
    }

    /// Evict the oldest filled plans until the cache holds at most
    /// [`ROWS_MAX`] rows and [`PLANS_MAX`] plans. `kept` stays. Totals and
    /// candidates use one inventory, since other fills can publish while
    /// this method runs.
    fn evict_past_bound(&self, kept: &PlanId) {
        let mut slots = lock(&self.0);
        prune_slots(&mut slots);
        let mut filled: Vec<_> = slots
            .iter()
            .filter_map(|(plan_id, slot)| {
                let (filled_at, results) = slot.get()?;
                Some((plan_id.clone(), *filled_at, results.rows()))
            })
            .collect();
        let rows: usize = filled.iter().map(|(_, _, rows)| rows).sum();
        let plans = filled.len();
        filled.sort_unstable_by_key(|(_, filled_at, _)| *filled_at);
        filled
            .into_iter()
            .filter(|(plan_id, _, _)| plan_id != kept)
            .scan((rows, plans), |(rows, plans), (plan_id, _, freed)| {
                if *rows <= ROWS_MAX && *plans <= PLANS_MAX {
                    return None;
                }
                let bound = if *rows > ROWS_MAX { "rows" } else { "plans" };
                *rows -= freed;
                *plans -= 1;
                Some((plan_id, freed, bound))
            })
            .for_each(|(plan_id, freed, bound)| {
                slots.remove(&plan_id);
                tracing::info!(
                    plan_id = %plan_id,
                    rows = freed,
                    bound,
                    "plan results evicted from the cache: a bound was reached"
                );
            });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::{StreamExt, TryStreamExt};

    use super::*;

    fn plan(n: u128) -> PlanId {
        PlanId::from_uuid(uuid::Uuid::from_u128(n))
    }

    fn job(n: u128) -> JobId {
        JobId::from_uuid(uuid::Uuid::from_u128(n))
    }

    fn row(job_id: JobId) -> ResultRow {
        ResultRow {
            job_id,
            benchmark_id: "prefill_throughput_256".to_string(),
            benchmark_type: "prefill_throughput".to_string(),
            metric: "prefill_throughput".to_string(),
            value: Some(1.0),
            value_stddev: None,
            unit: "tokens/sec".to_string(),
            failed: Some(false),
            failed_reason: None,
            model_descriptor: None,
            runtime_descriptor: None,
            runtime_descriptor_sha256: None,
            model_descriptor_sha256: None,
            model_vault_descriptor: None,
            model_flags: None,
            runtime_flags: None,
            benchmark_flags: None,
            requested_model_flags: None,
            requested_runtime_flags: None,
            requested_benchmark_flags: None,
            eval_metadata: None,
            client_extras: None,
            parameters: BTreeMap::new(),
            observations: BTreeMap::new(),
            client_id: None,
            device_name: None,
            device: BTreeMap::new(),
            client_version: None,
            submitted_at: None,
            scored_at: None,
            device_time_ms: None,
            failure_source: None,
            rescored_from: None,
            run_submitted_at: None,
            score_runtime_version: None,
            model_params_total_millions: None,
            model_params_active_millions: None,
        }
    }

    /// One job's run of `rows` rows, read as `read` says.
    fn results(rows: usize, read: ResultsRead) -> PlanJobResults {
        PlanJobResults {
            cells: Arc::from(vec![(job(1), None)]),
            read,
            runs: HashMap::from([(job(1), vec![row(job(1)); rows])]),
            history: History::default(),
        }
    }

    /// Fill `plan_id` through `cache`, counting the fills that run.
    async fn fill(
        cache: &PlanResultsCache,
        plan_id: &PlanId,
        fills: &AtomicUsize,
        filled: PlanJobResults,
    ) -> anyhow::Result<Arc<PlanJobResults>> {
        cache
            .get_or_fill(plan_id, || async {
                fills.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(filled)
            })
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_kept_plan_is_read_again_after_its_time() -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        let fills = AtomicUsize::new(0);
        let first = fill(&cache, &plan(1), &fills, results(1, ResultsRead::Ok)).await?;
        let again = fill(&cache, &plan(1), &fills, results(1, ResultsRead::Ok)).await?;
        assert!(Arc::ptr_eq(&first, &again));
        assert!(cache.get(&plan(1)).is_some());
        assert_eq!(fills.load(Ordering::SeqCst), 1);

        tokio::time::advance(KEEP_FOR).await;
        assert!(cache.get(&plan(1)).is_none());
        let late = fill(&cache, &plan(1), &fills, results(1, ResultsRead::Ok)).await?;
        assert!(!Arc::ptr_eq(&first, &late));
        assert_eq!(fills.load(Ordering::SeqCst), 2);
        Ok(())
    }

    /// Concurrent reads share one fill, also one that is not whole. The cache
    /// keeps only the whole one.
    #[rstest::rstest]
    #[case::ok(ResultsRead::Ok)]
    #[case::timed_out(ResultsRead::TimedOut)]
    #[case::failed(ResultsRead::Failed)]
    #[tokio::test(start_paused = true)]
    async fn concurrent_reads_share_one_fill(#[case] read: ResultsRead) -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        let fills = AtomicUsize::new(0);
        let plan_id = plan(1);
        let (a, b) = futures::join!(
            fill(&cache, &plan_id, &fills, results(1, read)),
            fill(&cache, &plan_id, &fills, results(1, read)),
        );
        assert!(Arc::ptr_eq(&a?, &b?), "{read:?}");
        assert_eq!(fills.load(Ordering::SeqCst), 1, "{read:?}");

        fill(&cache, &plan(1), &fills, results(1, read)).await?;
        let expected = if read == ResultsRead::Ok { 1 } else { 2 };
        assert_eq!(fills.load(Ordering::SeqCst), expected, "{read:?}");
        assert_eq!(cache.get(&plan(1)).is_some(), read == ResultsRead::Ok);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_fill_is_not_kept() -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        let failed = cache
            .get_or_fill(&plan(1), || async { Err::<PlanJobResults, _>("boom") })
            .await;
        assert!(failed.is_err());
        let fills = AtomicUsize::new(0);
        fill(&cache, &plan(1), &fills, results(1, ResultsRead::Ok)).await?;
        assert_eq!(fills.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// A read dropped during its fill leaves no slot that waits forever.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_fill_leaves_the_plan_readable() -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        let hung = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_fill(&plan(1), || async {
                std::future::pending::<Result<PlanJobResults, anyhow::Error>>().await
            }),
        )
        .await;
        assert!(hung.is_err());
        let fills = AtomicUsize::new(0);
        fill(&cache, &plan(1), &fills, results(1, ResultsRead::Ok)).await?;
        assert_eq!(fills.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// Empty plans still consume the completed-plan budget.
    #[tokio::test(start_paused = true)]
    async fn zero_row_plans_evict_the_oldest_entry() -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        let cache_ref = &cache;
        futures::stream::iter(1..=PLANS_MAX + 1)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each(|n| async move {
                let plan_id = plan(u128::try_from(n)?);
                cache_ref
                    .get_or_fill(&plan_id, || async {
                        Ok::<_, anyhow::Error>(results(0, ResultsRead::Ok))
                    })
                    .await?;
                tokio::time::advance(Duration::from_millis(1)).await;
                Ok(())
            })
            .await?;

        assert_eq!(lock(&cache.0).len(), PLANS_MAX);
        assert!(cache.get(&plan(1)).is_none());
        assert!(cache.get(&plan(2)).is_some());
        assert!(cache.get(&plan(u128::try_from(PLANS_MAX + 1)?)).is_some());
        Ok(())
    }

    /// A new fill drops expired results and cancelled, uninitialized slots.
    #[tokio::test(start_paused = true)]
    async fn a_new_fill_prunes_expired_and_abandoned_slots() -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        cache
            .get_or_fill(&plan(1), || async {
                Ok::<_, anyhow::Error>(results(0, ResultsRead::Ok))
            })
            .await?;
        tokio::time::advance(KEEP_FOR).await;

        let hung = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_fill(&plan(2), || async {
                std::future::pending::<Result<PlanJobResults, anyhow::Error>>().await
            }),
        )
        .await;
        assert!(hung.is_err());
        cache
            .get_or_fill(&plan(3), || async {
                Ok::<_, anyhow::Error>(results(0, ResultsRead::Ok))
            })
            .await?;

        assert_eq!(lock(&cache.0).len(), 1);
        assert!(cache.get(&plan(3)).is_some());
        Ok(())
    }

    /// Past the bound the plan with the oldest fill goes, one plan at a time,
    /// and a plan above the bound alone is never kept.
    #[tokio::test(start_paused = true)]
    async fn the_bound_evicts_the_oldest_plan() -> anyhow::Result<()> {
        let cache = PlanResultsCache::default();
        let fills = AtomicUsize::new(0);
        let half = ROWS_MAX / 2;
        fill(&cache, &plan(1), &fills, results(half, ResultsRead::Ok)).await?;
        fill(&cache, &plan(2), &fills, results(half, ResultsRead::Ok)).await?;
        fill(&cache, &plan(3), &fills, results(1, ResultsRead::Ok)).await?;
        assert!(cache.get(&plan(1)).is_none());
        assert!(cache.get(&plan(2)).is_some());
        assert!(cache.get(&plan(3)).is_some());

        fill(
            &cache,
            &plan(4),
            &fills,
            results(ROWS_MAX + 1, ResultsRead::Ok),
        )
        .await?;
        assert!(cache.get(&plan(4)).is_none());
        assert!(cache.get(&plan(2)).is_some());
        assert!(cache.get(&plan(3)).is_some());
        Ok(())
    }
}
