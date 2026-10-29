//! Implements the [`pipette_store`] traits over a directory tree, with the
//! file primitives they use. Writes stage a temp file and `rename(2)`, which
//! gives the job queue its atomic claim.

mod auth;
mod campaign_store;
mod dataset_cache;
mod model_params_store;
mod org_layout;
mod org_store;
pub mod storage;
mod tag_layout;
mod tag_store;
mod vault_store;

pub use auth::{LocalFsClientStore, LocalFsUserStore};
pub use org_store::LocalFsOrgStore;
pub use tag_store::LocalFsTagStore;

// The key index is handed out as `Arc<dyn KeyIndexStore>`, so the concrete type
// stays inside this crate.
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
pub(crate) use auth::LocalFsKeyIndexStore;
pub(crate) use campaign_store::LocalFsCampaignStore;
use chrono::{DateTime, Utc};
pub(crate) use model_params_store::LocalFsModelParamsStore;

use pipette_core::benchmark::{self, Benchmark};
use pipette_core::config::Config;
use pipette_core::denial::DenialRecord;
use pipette_core::eval_sample_result::{self, EvalSampleResult};
use pipette_core::parquet_utils::WriterOpts;
use pipette_core::plan::{PlanManifest, PlanSource, SettledOutcome};
use pipette_core::tmp_file::TmpFileGuard;
use pipette_core::todo_filename::{
    avail_filename, denied_job_prefix, denied_marker_filename, eligible_filename,
    job_plan_marker_filename, leased_key, lost_lease_filename, parse_denied_marker,
    parse_eligible_filename, parse_job_plan_marker_filename, parse_leased_key,
    parse_lost_lease_filename, tmp_filename,
};
use pipette_core::types::id_from_key;
use pipette_core::warehouse::{self, JobMetrics, MetricRow};
use pipette_plan_types::ExpiresAt;
use pipette_plan_types::IdError;
use pipette_plan_types::JobId;
use pipette_plan_types::PlanId;
use pipette_plan_types::PlanStatus;
use pipette_plan_types::{BenchmarkId, ClientId, OrgId, RuntimeOutput};
use pipette_store::{
    BacklogSnapshot, CampaignBacklogSnapshot, CatalogStore, ClaimResult, ClientStore,
    ContentKeyCas, ContentKeyClaim, ContentKeyError, ContentKeyRecord, ContentKeyVersion,
    CreateOutcome, EvalSampleResultStore, FinalizeProbe, JobPlanMarker, JobState,
    ORG_BENCHMARKS_PREFIX, OutcomeProbe, Page, PendingSourceMarker, PlanFilter, PlanPage,
    PlanStore, RecycleResult, RedateResult, RenewLeaseResult, ScoreQueueStage, Stores,
    SubmissionRecord, SubmissionStore, SuspensionRecord, TodoStore, UnreadableObjectError,
    UserStore, WarehouseStore, find_outcomes_per_job, is_content_digest, page_matching_plans,
};

pub fn build_local_fs_client_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<Arc<dyn ClientStore>> {
    let data_dir = storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_client_store requires local_fs backend"))?;
    std::fs::create_dir_all(data_dir.join("clients"))?;
    Ok(Arc::new(LocalFsClientStore::new(data_dir.clone())))
}

/// Account directories are created on write, as the claim's exclusive create
/// needs.
pub fn build_local_fs_user_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<Arc<dyn UserStore>> {
    let data_dir = storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_user_store requires local_fs backend"))?;
    Ok(Arc::new(LocalFsUserStore::new(data_dir.clone())))
}

/// The `todo` store is also rooted at `[storage]`: only `build_stores` in the
/// binary honors `[todo_storage]`.
pub fn build_local_fs_stores(config: &Config) -> anyhow::Result<Stores> {
    let data_dir = config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_stores requires local_fs backend"))?;

    let writer_opts = config.writer_opts();

    let clients_dir = data_dir.join("clients");
    let submissions_dir = data_dir.join("submissions");
    let warehouse_dir = data_dir.join("warehouse");
    let warehouse_results_dir = warehouse_dir.join("results");
    let eval_sample_results_dir = warehouse_dir.join("eval_sample_results");
    std::fs::create_dir_all(&clients_dir)?;
    std::fs::create_dir_all(submissions_dir.join("incoming"))?;
    std::fs::create_dir_all(submissions_dir.join("processed"))?;
    std::fs::create_dir_all(&warehouse_results_dir)?;
    std::fs::create_dir_all(&eval_sample_results_dir)?;
    create_plan_dirs(data_dir)?;

    Ok(Stores {
        catalog: Arc::new(LocalFsCatalogStore::new(data_dir)),
        clients: Arc::new(LocalFsClientStore::new(data_dir.clone())),
        users: Arc::new(LocalFsUserStore::new(data_dir.clone())),
        orgs: Arc::new(LocalFsOrgStore::new(data_dir.clone())),
        tags: Arc::new(LocalFsTagStore::new(data_dir.clone())),
        key_index: Arc::new(LocalFsKeyIndexStore::new(data_dir.clone())),
        submissions: Arc::new(LocalFsSubmissionStore::new(submissions_dir)),
        warehouse: Arc::new(LocalFsWarehouseStore::new(
            warehouse_results_dir,
            config.warehouse_read_days,
            config.warehouse_max_rows_per_part,
            writer_opts,
        )),
        eval_sample_results: Arc::new(LocalFsEvalSampleResultStore::new(
            eval_sample_results_dir,
            writer_opts,
        )),
        todo: build_local_fs_todo_store(&config.storage)?,
        plans: Arc::new(LocalFsPlanStore::new(data_dir)),
        campaigns: Arc::new(LocalFsCampaignStore::new(data_dir)),
        model_params: Arc::new(LocalFsModelParamsStore::new(data_dir)),
        audit: None,
        descriptor_catalog: None,
        catalog_import_runs: None,
        vault: Arc::new(vault_store::LocalFsVaultStore::new(data_dir)),
        dataset_cache: Arc::new(dataset_cache::LocalFsDatasetCacheStore::new(
            data_dir.join("dataset-cache").join("artifacts"),
        )),
    })
}

/// The per-job `job-plans/` and `job-plan-links/` directories are created on
/// first write instead, like `leased/{client_id}/`.
fn create_plan_dirs(data_dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir.join("plans"))?;
    std::fs::create_dir_all(data_dir.join("campaigns"))?;
    std::fs::create_dir_all(data_dir.join("cancelled_plans"))?;
    std::fs::create_dir_all(data_dir.join("content-keys").join("v1"))?;
    std::fs::create_dir_all(data_dir.join("job-plans"))?;
    std::fs::create_dir_all(data_dir.join("plan-sources"))?;
    std::fs::create_dir_all(data_dir.join("plan-sources-pending"))?;
    std::fs::create_dir_all(data_dir.join("job-sources"))?;
    std::fs::create_dir_all(data_dir.join("job-plan-links"))?;
    Ok(())
}

/// `storage` must be local_fs. Unlike [`build_local_fs_stores`], this leaves
/// the queue's `todo/` tree alone.
pub fn build_local_fs_plan_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<Arc<dyn PlanStore>> {
    let data_dir = storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_plan_store requires local_fs backend"))?;
    create_plan_dirs(data_dir)?;
    Ok(Arc::new(LocalFsPlanStore::new(data_dir)))
}

/// `storage` must be a local_fs backend.
pub fn build_local_fs_todo_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<Arc<dyn TodoStore>> {
    let todo_dir = storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_todo_store requires local_fs backend"))?
        .join("todo");
    std::fs::create_dir_all(todo_dir.join("avail"))?;
    std::fs::create_dir_all(todo_dir.join("leased"))?;
    std::fs::create_dir_all(todo_dir.join("denied"))?;
    std::fs::create_dir_all(todo_dir.join("eligible").join("clients"))?;
    std::fs::create_dir_all(todo_dir.join("pending-reindex"))?;
    std::fs::create_dir_all(todo_dir.join("pending-reindex-jobs"))?;
    std::fs::create_dir_all(todo_dir.join("lost"))?;
    std::fs::create_dir_all(todo_dir.join("tmp"))?;
    std::fs::create_dir_all(todo_dir.join("suspended"))?;
    std::fs::create_dir_all(todo_dir.join("liveness"))?;
    Ok(Arc::new(LocalFsTodoStore::new(todo_dir)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `f` on the blocking pool inside the caller's span: a span is
/// thread-local, so it would not follow the closure on its own.
///
/// The blocking task runs a boxed job, and the job sends the result of `f`
/// back on a oneshot channel. The task type is the same for every call, so
/// tokio's blocking-task code compiles once for the crate. A task that holds
/// `f` or its result directly makes a separate copy of that code for each
/// call site.
async fn run_blocking<T, F>(f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let span = tracing::Span::current();
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn_blocking_job(Box::new(move || {
        // The receiver is gone only if the caller dropped this future, and
        // then nothing waits for the result.
        let _ = tx.send(span.in_scope(f));
    }))
    .await?;
    rx.await
        .map_err(|e| anyhow::anyhow!("blocking task ended without a result: {e}"))?
}

async fn spawn_blocking_job(job: Box<dyn FnOnce() + Send>) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
}

async fn remove_file_idempotent(path: PathBuf) -> anyhow::Result<()> {
    run_blocking(move || match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    })
    .await
}

/// A missing or corrupt file reads as empty ([`pipette_store::parse_key_set`]).
async fn read_key_set<T>(path: PathBuf, what: &'static str) -> anyhow::Result<HashSet<T>>
where
    T: serde::de::DeserializeOwned + Eq + std::hash::Hash + Send + 'static,
{
    run_blocking(move || match std::fs::read(&path) {
        Ok(bytes) => Ok(pipette_store::parse_key_set(&bytes, what)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e.into()),
    })
    .await
}

async fn write_key_set<T: serde::Serialize + Sync>(
    path: PathBuf,
    set: &HashSet<T>,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(set)?;
    run_blocking(move || Ok(std::fs::write(&path, &bytes)?)).await
}

/// Replace the snapshot at `path` atomically through a sibling temp file, so a
/// console read never sees a truncated snapshot.
async fn replace_snapshot(path: PathBuf, bytes: Vec<u8>) -> anyhow::Result<()> {
    run_blocking(move || {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("snapshot path has no file name")?;
        let tmp = path.with_file_name(format!("{name}.tmp-{}", uuid::Uuid::new_v4()));
        let guard = TmpFileGuard::new(tmp.clone());
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        guard.disarm();
        Ok(())
    })
    .await
}

/// `Ok(None)` for a path that vanished mid-walk. A failed read propagates: a
/// partial view is not an answer.
fn vanished_is_absent<T>(result: std::io::Result<T>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Drop a now-empty directory, matching S3 Express, which reclaims one itself.
/// Best effort: races with other writers are expected, other failures logged.
fn drop_empty_dir(dir: &Path) {
    match std::fs::remove_dir(dir) {
        Ok(()) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::DirectoryNotEmpty
                    | std::io::ErrorKind::AlreadyExists
                    | std::io::ErrorKind::NotFound
            ) => {}
        Err(e) => tracing::warn!(
            path = %dir.display(),
            error = %e,
            "failed to remove empty directory; it is left behind"
        ),
    }
}

/// Remove a staging file once its write has landed or failed. Any failure but
/// `NotFound` leaves an inert file behind, and is logged.
fn remove_staged(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to remove staged file; it is left behind"
        ),
    }
}

/// A listed entry, or `None` after logging one that could not be read.
/// `NotFound` is a directory removed mid-walk, and is not logged.
fn readable_entry(
    dir: &Path,
    entry: std::io::Result<std::fs::DirEntry>,
) -> Option<std::fs::DirEntry> {
    match entry {
        Ok(entry) => Some(entry),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "skipping unreadable directory entry"
            );
            None
        }
    }
}

/// Whether `entry` passes `is`; `false` when its type cannot be read, which is
/// logged unless the entry was removed mid-walk.
fn entry_is(entry: &std::fs::DirEntry, is: fn(&std::fs::FileType) -> bool) -> bool {
    match entry.file_type() {
        Ok(file_type) => is(&file_type),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!(
                path = %entry.path().display(),
                error = %e,
                "skipping directory entry of unreadable type"
            );
            false
        }
    }
}

/// The name of an entry that holds a JSON body, else `None`. A non-UTF-8 name
/// is no key this store writes, so it is foreign cruft too.
fn json_name(entry: &std::fs::DirEntry) -> Option<String> {
    entry
        .file_name()
        .into_string()
        .ok()
        .filter(|name| is_json_name(name))
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the store writes the lowercase suffix itself; any other case is not one of its keys"
)]
pub(crate) fn is_json_name(name: &str) -> bool {
    name.ends_with(".json")
}

// ---------------------------------------------------------------------------
// CatalogStore
// ---------------------------------------------------------------------------

/// The public catalog under `{data_dir}/benchmarks/`, and each organization's
/// under `{data_dir}/org-benchmarks/{org_id}/`.
#[derive(Clone)]
pub(crate) struct LocalFsCatalogStore {
    benchmarks_dir: PathBuf,
    org_benchmarks_dir: PathBuf,
}

impl LocalFsCatalogStore {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            benchmarks_dir: data_dir.join("benchmarks"),
            org_benchmarks_dir: data_dir.join(ORG_BENCHMARKS_PREFIX),
        }
    }

    fn org_dir(&self, org_id: &OrgId) -> PathBuf {
        self.org_benchmarks_dir.join(org_id.as_str())
    }

    fn org_entry_path(&self, org_id: &OrgId, benchmark_id: &BenchmarkId) -> PathBuf {
        self.org_dir(org_id).join(format!("{benchmark_id}.toml"))
    }
}

#[async_trait]
impl CatalogStore for LocalFsCatalogStore {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let benchmarks_dir = self.benchmarks_dir.clone();
        let load = run_blocking(move || benchmark::read_catalog(&benchmarks_dir)).await?;
        load.log_errors();
        Ok(load.catalog)
    }

    async fn get_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<Option<Benchmark>> {
        let path = self.org_entry_path(org_id, benchmark_id);
        let org_id = org_id.clone();
        let benchmark_id = benchmark_id.clone();
        // Read and map the miss rather than test-then-read: an entry deleted
        // between the two is absent, not a fault.
        run_blocking(move || {
            let unreadable = |reason: String| UnreadableObjectError {
                key: path.display().to_string(),
                reason,
            };
            match std::fs::read_to_string(&path) {
                Ok(content) => Benchmark::from_toml(benchmark_id.as_str(), &content)
                    .map(|entry| Some(entry.private_to(org_id)))
                    .map_err(|e| unreadable(format!("{e:#}")).into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                // Not UTF-8: the content's fault, as it is on the S3 backend.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    Err(unreadable(e.to_string()).into())
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn put_org_benchmark(&self, org_id: &OrgId, benchmark: &Benchmark) -> anyhow::Result<()> {
        let dir = self.org_dir(org_id);
        let path = self.org_entry_path(org_id, benchmark.benchmark_id());
        let body = benchmark.to_toml()?;
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Atomic replace, so a read never sees a truncated entry. The
            // `.tmp-` name has no `.toml` extension, so a listing skips it.
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let guard = TmpFileGuard::new(tmp.clone());
            std::fs::write(&tmp, body)?;
            std::fs::rename(&tmp, &path)?;
            guard.disarm();
            Ok(())
        })
        .await
    }

    async fn delete_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<()> {
        remove_file_idempotent(self.org_entry_path(org_id, benchmark_id)).await
    }

    async fn list_org_benchmark_ids(&self) -> anyhow::Result<Vec<(OrgId, BenchmarkId)>> {
        let root = self.org_benchmarks_dir.clone();
        run_blocking(move || {
            let orgs = match std::fs::read_dir(&root) {
                Ok(orgs) => orgs,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            let mut ids = Vec::new();
            for org in orgs {
                let org = org?;
                if !org.file_type()?.is_dir() {
                    continue;
                }
                let Some(org_id) = org.file_name().to_str().and_then(|s| {
                    OrgId::try_new(s)
                        .inspect_err(|e| {
                            tracing::warn!(
                                path = %org.path().display(),
                                error = %e,
                                "skipping org benchmark entry with invalid org_id"
                            );
                        })
                        .ok()
                }) else {
                    continue;
                };
                ids.extend(
                    org_entry_ids(&org.path())?
                        .into_iter()
                        .map(|id| (org_id.clone(), id)),
                );
            }
            Ok(ids)
        })
        .await
    }

    /// One directory listing, not the whole keyspace's.
    async fn list_benchmark_ids_of(&self, org_id: &OrgId) -> anyhow::Result<Vec<BenchmarkId>> {
        let dir = self.org_dir(org_id);
        run_blocking(move || {
            let mut ids = match org_entry_ids(&dir) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                ids => ids?,
            };
            ids.sort();
            Ok(ids)
        })
        .await
    }
}

/// A file that names no valid id is skipped.
fn org_entry_ids(dir: &Path) -> std::io::Result<Vec<BenchmarkId>> {
    std::fs::read_dir(dir)?.try_fold(Vec::new(), |mut ids, entry| {
        let path = entry?.path();
        let stem = path
            .extension()
            .filter(|ext| *ext == "toml")
            .and_then(|_| path.file_stem()?.to_str());
        if let Some(stem) = stem {
            match BenchmarkId::try_new(stem) {
                Ok(id) => ids.push(id),
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping org benchmark entry with invalid benchmark_id"
                ),
            }
        }
        Ok(ids)
    })
}

// ---------------------------------------------------------------------------
// PlanStore
// ---------------------------------------------------------------------------

/// Plan manifests as `{plans_dir}/{plan_id}.json`. Cancel markers and the other
/// plan keyspaces are sibling directories, so `list_plans` never sees them.
#[derive(Clone)]
pub(crate) struct LocalFsPlanStore {
    plans_dir: PathBuf,
    cancelled_dir: PathBuf,
    content_keys_dir: PathBuf,
    job_plans_dir: PathBuf,
    sources_dir: PathBuf,
    pending_sources_dir: PathBuf,
    job_sources_dir: PathBuf,
    job_plan_links_dir: PathBuf,
}

impl LocalFsPlanStore {
    pub(crate) fn new(data_dir: &std::path::Path) -> Self {
        Self {
            plans_dir: data_dir.join("plans"),
            cancelled_dir: data_dir.join("cancelled_plans"),
            content_keys_dir: data_dir.join("content-keys").join("v1"),
            job_plans_dir: data_dir.join("job-plans"),
            sources_dir: data_dir.join("plan-sources"),
            pending_sources_dir: data_dir.join("plan-sources-pending"),
            job_sources_dir: data_dir.join("job-sources"),
            job_plan_links_dir: data_dir.join("job-plan-links"),
        }
    }

    async fn list_plans_by_status(
        &self,
        status: Option<PlanStatus>,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage> {
        let dir = self.plans_dir.clone();
        let after = after.map(|id| id.as_str().to_string());
        run_blocking(move || {
            page_flat_keyspace(
                &dir,
                after.as_deref(),
                limit,
                PlanId::try_new,
                |manifest: &PlanManifest| {
                    // Applied after the body is read, so a selective `status` keeps
                    // the page reading rather than returning short.
                    status.is_none_or(|wanted| manifest.status == wanted)
                },
            )
        })
        .await
    }

    fn job_source_path(&self, job_id: &JobId) -> PathBuf {
        self.job_sources_dir.join(format!("{job_id}.json"))
    }

    fn job_plan_links_dir_for(&self, job_id: &JobId) -> PathBuf {
        self.job_plan_links_dir.join(job_id.as_str())
    }

    fn source_path(&self, plan_id: &PlanId) -> PathBuf {
        self.sources_dir.join(format!("{}.toml", plan_id.as_str()))
    }

    /// Pending marker path — the bare `plan_id`, like a cancel marker.
    fn pending_source_marker_path(&self, plan_id: &PlanId) -> PathBuf {
        self.pending_sources_dir.join(plan_id.as_str())
    }

    fn manifest_path(&self, plan_id: &PlanId) -> PathBuf {
        self.plans_dir.join(format!("{}.json", plan_id.as_str()))
    }

    /// Marker path — the bare `plan_id`, no extension, mirroring the other
    /// empty-marker trees (`todo/denied/`, `tags-index/`).
    fn cancel_marker_path(&self, plan_id: &PlanId) -> PathBuf {
        self.cancelled_dir.join(plan_id.as_str())
    }

    /// A digest is lowercase hex (filtered by [`is_content_digest`] on read),
    /// so it is a safe path segment.
    fn content_key_path(&self, digest: &str) -> PathBuf {
        self.content_keys_dir.join(digest)
    }

    fn job_plans_dir_for(&self, job_id: &JobId) -> PathBuf {
        self.job_plans_dir.join(job_id.as_str())
    }
}

/// The compare-and-swap token: a digest of the bytes read, so an identical
/// rewrite reads as unchanged.
fn bytes_version(bytes: &[u8]) -> ContentKeyVersion {
    use sha2::{Digest, Sha256};
    ContentKeyVersion::Digest(hex::encode(Sha256::digest(bytes)))
}

fn read_content_key(path: &std::path::Path) -> anyhow::Result<Option<ContentKeyEntry>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some((
            serde_json::from_slice(&bytes)?,
            bytes_version(&bytes),
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

type ContentKeyEntry = (ContentKeyRecord, ContentKeyVersion);

/// One page of a flat `{id}.json` keyspace in ascending key order. Shared by
/// [`PlanStore::list_plans`] and [`CampaignStore::list_campaigns`] so their
/// cursor and unreadable-object handling cannot drift apart.
fn page_flat_keyspace<T, Id, E>(
    dir: &Path,
    after: Option<&str>,
    limit: NonZeroUsize,
    parse_id: fn(String) -> Result<Id, E>,
    admit: impl Fn(&T) -> bool,
) -> anyhow::Result<Page<T, Id>>
where
    T: serde::de::DeserializeOwned,
    Id: std::fmt::Display + Clone,
    IdError: From<E>,
{
    let mut page = Page::default();
    if !dir.exists() {
        return Ok(page);
    }
    // `read_dir` order is arbitrary; the sort makes key order a contract. A
    // `.tmp-` file or a stem outside the id charset is not an object here.
    let mut candidates: Vec<(String, Id)> = std::fs::read_dir(dir)?
        .map(|entry| -> anyhow::Result<Option<_>> {
            let entry = entry?;
            let Ok(leaf) = entry.file_name().into_string() else {
                return Ok(None);
            };
            Ok(pipette_store::flat_keyspace_id(&leaf, parse_id).map(|id| (leaf, id)))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
    // `after` is a bare id; suffix it to compare like for like.
    if let Some(after) = after.map(|id| format!("{id}.json")) {
        candidates.retain(|(leaf, _)| *leaf > after);
    }

    // A parse failure is reported: dropping it would answer "no such record",
    // failing would hide the rest. Any other read failure but `NotFound` is
    // fatal: we do not know whether the record exists.
    let mut examined = 0usize;
    for (leaf, id) in &candidates {
        examined += 1;
        let path = dir.join(leaf);
        match std::fs::read(&path) {
            Ok(data) => match serde_json::from_slice::<T>(&data) {
                Ok(record) if admit(&record) => page.items.push(record),
                Ok(_) => {}
                Err(e) => page.unreadable.push(UnreadableObjectError {
                    key: path.display().to_string(),
                    reason: e.to_string(),
                }),
            },
            // Removed since `read_dir`: as if listed a moment later.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if page.items.len() == limit.get() {
            // The last key examined, so the next page starts past every object
            // this one decided — including the broken ones it reported.
            page.resume_after = (examined < candidates.len()).then(|| id.clone());
            break;
        }
    }
    Ok(page)
}

#[async_trait]
impl PlanStore for LocalFsPlanStore {
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
        let dir = self.plans_dir.clone();
        let path = self.manifest_path(&manifest.plan_id);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Temp sibling then rename, so a concurrent reader never sees a
            // truncated manifest. The `.tmp-` name has no `.json` extension, so
            // `list_plans` skips it.
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let guard = TmpFileGuard::new(tmp.clone());
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
            guard.disarm();
            Ok(())
        })
        .await
    }

    async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
        let path = self.manifest_path(plan_id);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list_matching_plans(
        &self,
        filter: &PlanFilter,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage> {
        let status = filter.status;
        page_matching_plans(filter, after, limit, move |after, limit| async move {
            self.list_plans_by_status(status, after.as_ref(), limit)
                .await
        })
        .await
    }

    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        let dir = self.cancelled_dir.clone();
        let path = self.cancel_marker_path(plan_id);
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Empty marker: nothing to tear, so a plain write suffices.
            Ok(std::fs::write(&path, b"")?)
        })
        .await
    }

    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
        let path = self.cancel_marker_path(plan_id);
        // `try_exists` rather than `exists`: a permissions/IO failure must
        // propagate, not read as "not cancelled".
        run_blocking(move || Ok(path.try_exists()?)).await
    }

    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
        let dir = self.cancelled_dir.clone();
        run_blocking(move || {
            // `try_exists`: an IO failure read as "nothing to cancel" would
            // skip teardown on every pass.
            if !dir.try_exists()? {
                return Ok(Vec::new());
            }
            // Read errors propagate (teardown must not lose a cancel); an
            // unparseable name is cruft, warned and skipped.
            std::fs::read_dir(&dir)?
                .map(|e| anyhow::Ok(e?.file_name().to_string_lossy().into_owned()))
                .filter_map(|r| {
                    r.map(|name| match PlanId::try_new(&name) {
                        Ok(plan_id) => Some(plan_id),
                        Err(_) => {
                            tracing::warn!(key = %name, "skipping unparseable cancel marker");
                            None
                        }
                    })
                    .transpose()
                })
                .collect()
        })
        .await
    }

    async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        remove_file_idempotent(self.cancel_marker_path(plan_id)).await
    }

    async fn get_content_key(
        &self,
        digest: &str,
    ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
        let path = self.content_key_path(digest);
        run_blocking(move || read_content_key(&path)).await
    }

    async fn create_content_key(
        &self,
        digest: &str,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyClaim> {
        let dir = self.content_keys_dir.clone();
        let path = self.content_key_path(digest);
        let bytes = serde_json::to_vec_pretty(record)?;
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Stage in full, then `hard_link` under the digest: `AlreadyExists`
            // makes it create-if-absent, and a reader never sees a partial
            // record. The guard drops the staging name even on success.
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let _guard = TmpFileGuard::new(tmp.clone());
            std::fs::write(&tmp, &bytes)?;
            // Twice: the GC may delete the record between the failed link and
            // the read. A digest that vanishes twice propagates.
            for _ in 0..2 {
                match std::fs::hard_link(&tmp, &path) {
                    Ok(()) => return Ok(ContentKeyClaim::Created),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        if let Some((record, version)) = read_content_key(&path)? {
                            return Ok(ContentKeyClaim::AlreadyExists(record, version));
                        }
                        tracing::debug!(
                            path = %path.display(),
                            "content key removed after a refused create"
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(ContentKeyError::ConcurrentChurn {
                path: path.display().to_string(),
            }
            .into())
        })
        .await
    }

    async fn replace_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyCas> {
        let dir = self.content_keys_dir.clone();
        let path = self.content_key_path(digest);
        let expected = version.clone();
        let bytes = serde_json::to_vec_pretty(record)?;
        run_blocking(move || {
            // Read-check-write: refuse a record changed since the caller read
            // it. Not atomic, but two racing writers each leave a valid record
            // naming a live job; only overwriting an unread record is ruled out.
            let Some((_, actual)) = read_content_key(&path)? else {
                return Ok(ContentKeyCas::Lost);
            };
            if actual != expected {
                return Ok(ContentKeyCas::Lost);
            }
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let guard = TmpFileGuard::new(tmp.clone());
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
            guard.disarm();
            Ok(ContentKeyCas::Applied)
        })
        .await
    }

    async fn delete_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
    ) -> anyhow::Result<ContentKeyCas> {
        let path = self.content_key_path(digest);
        let expected = version.clone();
        run_blocking(move || {
            // Same read-check-write window as `replace_content_key`; a lost
            // delete leaves a record a later GC collects.
            let Some((_, actual)) = read_content_key(&path)? else {
                return Ok(ContentKeyCas::Lost);
            };
            if actual != expected {
                return Ok(ContentKeyCas::Lost);
            }
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(ContentKeyCas::Applied),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ContentKeyCas::Lost),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn list_content_keys(
        &self,
    ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
        let dir = self.content_keys_dir.clone();
        run_blocking(move || {
            if !dir.try_exists()? {
                return Ok(Vec::new());
            }
            // A non-digest name (a `.tmp-` sibling included) is cruft, skipped
            // silently.
            std::fs::read_dir(&dir)?
                .map(|entry| -> anyhow::Result<Option<_>> {
                    let name = entry?.file_name().to_string_lossy().into_owned();
                    if !is_content_digest(&name) {
                        return Ok(None);
                    }
                    Ok(read_content_key(&dir.join(&name))?
                        .map(|(record, version)| (name, record, version)))
                })
                .filter_map(Result::transpose)
                .collect()
        })
        .await
    }

    async fn write_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let dir = self.job_plans_dir_for(job_id);
        let path = dir.join(job_plan_marker_filename(plan_id, expires_at));
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            match std::fs::write(&path, b"") {
                Ok(()) => Ok(()),
                // A concurrent delete of the job's last marker took the
                // directory. Retry once: it is removed only while empty.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        path = %path.display(),
                        "job-plans directory removed concurrently; retrying the write"
                    );
                    std::fs::create_dir_all(&dir)?;
                    Ok(std::fs::write(&path, b"")?)
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn list_job_plan_markers(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>> {
        let dir = self.job_plans_dir_for(job_id);
        run_blocking(move || {
            // `try_exists`: "no member" licenses teardown to retire the job, so
            // an IO failure must propagate.
            if !dir.try_exists()? {
                return Ok(Vec::new());
            }
            std::fs::read_dir(&dir)?
                .map(|entry| -> anyhow::Result<Option<_>> {
                    let name = entry?.file_name().to_string_lossy().into_owned();
                    match parse_job_plan_marker_filename(&name) {
                        Ok(parsed) => Ok(Some(parsed)),
                        Err(e) => {
                            tracing::warn!(key = %name, error = %e,
                                "skipping unparseable job-plans marker");
                            Ok(None)
                        }
                    }
                })
                .filter_map(Result::transpose)
                .collect()
        })
        .await
    }

    async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
        let root = self.job_plans_dir.clone();
        run_blocking(move || {
            if !root.try_exists()? {
                return Ok(Vec::new());
            }
            // A missed marker fails safe: teardown rereads the per-job listing
            // before any retire.
            let mut markers = Vec::new();
            for job_entry in std::fs::read_dir(&root)? {
                let Some(job_entry) = vanished_is_absent(job_entry)? else {
                    continue;
                };
                let job_name = job_entry.file_name().to_string_lossy().into_owned();
                let Ok(job_id) = JobId::try_new(&job_name) else {
                    tracing::warn!(key = %job_name, "skipping unparseable job-plans directory");
                    continue;
                };
                // The job's last marker went, taking the directory.
                let Some(entries) = vanished_is_absent(std::fs::read_dir(job_entry.path()))? else {
                    continue;
                };
                for marker in entries {
                    let Some(marker) = vanished_is_absent(marker)? else {
                        continue;
                    };
                    let name = marker.file_name().to_string_lossy().into_owned();
                    let Ok((plan_id, expires_at)) = parse_job_plan_marker_filename(&name) else {
                        tracing::warn!(key = %name, "skipping unparseable job-plans marker");
                        continue;
                    };
                    // Collected between this listing and the stat.
                    let Some(written_at) =
                        vanished_is_absent(marker.metadata().and_then(|m| m.modified()))?
                    else {
                        continue;
                    };
                    markers.push(JobPlanMarker {
                        job_id: job_id.clone(),
                        plan_id,
                        expires_at,
                        written_at: written_at.into(),
                    });
                }
            }
            Ok(markers)
        })
        .await
    }

    async fn delete_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let dir = self.job_plans_dir_for(job_id);
        let path = dir.join(job_plan_marker_filename(plan_id, expires_at));
        remove_file_idempotent(path).await?;
        // Drop the emptied job directory, or one per job ever minted piles up
        // for every maintenance run to walk. `remove_dir` takes only an empty
        // one, and `write_job_plan_marker` retries after it.
        run_blocking(move || {
            drop_empty_dir(&dir);
            Ok(())
        })
        .await
    }

    async fn put_plan_source(
        &self,
        plan_id: &PlanId,
        source: &PlanSource,
    ) -> anyhow::Result<CreateOutcome> {
        if source.names_a_token() {
            anyhow::bail!(
                "refused to store the source of plan {plan_id}: it names an `auth_token`"
            );
        }
        let dir = self.sources_dir.clone();
        let path = self.source_path(plan_id);
        let text = source.text().to_owned();
        // The `.tmp-` staging name has no `.toml` extension, so it never reads
        // as a source.
        run_blocking(move || storage::create_complete(&dir, &path, text.as_bytes())).await
    }

    async fn get_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanSource>> {
        let path = self.source_path(plan_id);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => {
                let text = String::from_utf8(data)
                    .with_context(|| format!("plan source {} is not UTF-8", path.display()))?;
                Ok(Some(PlanSource::new(text)?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        remove_file_idempotent(self.source_path(plan_id)).await
    }

    async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        let dir = self.pending_sources_dir.clone();
        let path = self.pending_source_marker_path(plan_id);
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Empty, so a plain write cannot tear, as for a cancel marker.
            Ok(std::fs::write(&path, b"")?)
        })
        .await
    }

    async fn list_pending_source_markers(&self) -> anyhow::Result<Vec<PendingSourceMarker>> {
        let dir = self.pending_sources_dir.clone();
        run_blocking(move || {
            if !dir.try_exists()? {
                return Ok(Vec::new());
            }
            // A read failure propagates: the sweep deletes sources, so a partial
            // view must not pass for a whole one.
            std::fs::read_dir(&dir)?
                .map(|entry| -> anyhow::Result<Option<PendingSourceMarker>> {
                    let Some(entry) = vanished_is_absent(entry)? else {
                        return Ok(None);
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let Ok(plan_id) = PlanId::try_new(&name) else {
                        tracing::warn!(key = %name, "skipping unparseable pending source marker");
                        return Ok(None);
                    };
                    let Some(written_at) =
                        vanished_is_absent(entry.metadata().and_then(|m| m.modified()))?
                    else {
                        return Ok(None);
                    };
                    Ok(Some(PendingSourceMarker {
                        plan_id,
                        written_at: written_at.into(),
                    }))
                })
                .filter_map(Result::transpose)
                .collect()
        })
        .await
    }

    async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        remove_file_idempotent(self.pending_source_marker_path(plan_id)).await
    }

    async fn put_job_source(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        let dir = self.job_sources_dir.clone();
        let path = self.job_source_path(job_id);
        let bytes = serde_json::to_vec(body)?;
        // The `.tmp-` staging name has no `.json` extension, so it never reads
        // as a job source.
        run_blocking(move || storage::create_complete(&dir, &path, &bytes)).await
    }

    async fn get_job_source(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let path = self.job_source_path(job_id);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).map(Some).map_err(|e| {
                UnreadableObjectError {
                    key: path.display().to_string(),
                    reason: e.to_string(),
                }
                .into()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn put_job_plan_link(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
    ) -> anyhow::Result<CreateOutcome> {
        let dir = self.job_plan_links_dir_for(job_id);
        let path = dir.join(plan_id.as_str());
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Empty link: `create_new` is the create-if-absent step, and there
            // is no content to tear.
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path);
            match opened {
                Ok(_) => Ok(CreateOutcome::Created),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(CreateOutcome::AlreadyExists)
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
        let dir = self.job_plan_links_dir_for(job_id);
        run_blocking(move || {
            // `try_exists`: an IO failure must propagate, not report that no
            // plan holds the job.
            if !dir.try_exists()? {
                return Ok(Vec::new());
            }
            std::fs::read_dir(&dir)?
                .map(|entry| -> anyhow::Result<Option<PlanId>> {
                    let name = entry?.file_name().to_string_lossy().into_owned();
                    match PlanId::try_new(&name) {
                        Ok(plan_id) => Ok(Some(plan_id)),
                        Err(e) => {
                            tracing::warn!(key = %name, error = %e,
                                "skipping unparseable job-plan-links key");
                            Ok(None)
                        }
                    }
                })
                .filter_map(Result::transpose)
                .collect()
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// SubmissionStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocalFsSubmissionStore {
    incoming_dir: PathBuf,
    processed_dir: PathBuf,
    unverified_dir: PathBuf,
    score_queue_dir: PathBuf,
    runtime_output_dir: PathBuf,
}

impl LocalFsSubmissionStore {
    /// Every subdirectory derives from `submissions_dir` explicitly.
    pub fn new(submissions_dir: PathBuf) -> Self {
        Self {
            incoming_dir: submissions_dir.join("incoming"),
            processed_dir: submissions_dir.join("processed"),
            unverified_dir: submissions_dir.join("unverified"),
            score_queue_dir: submissions_dir.join(ScoreQueueStage::ROOT),
            runtime_output_dir: submissions_dir.join("runtime-output"),
        }
    }

    fn stage_dir(&self, stage: ScoreQueueStage) -> PathBuf {
        self.score_queue_dir.join(stage.leaf())
    }

    /// From the first place that holds it, in [`SubmissionStore::find_outcomes`]
    /// order.
    async fn find_outcome(&self, job_id: &JobId) -> anyhow::Result<Option<SettledOutcome>> {
        let incoming = storage::submission_path(&self.incoming_dir, job_id);
        let to_do = storage::submission_path(&self.stage_dir(ScoreQueueStage::ToDo), job_id);
        let parked = storage::submission_path(&self.stage_dir(ScoreQueueStage::Parked), job_id);
        let to_finalize =
            storage::submission_path(&self.stage_dir(ScoreQueueStage::ToFinalize), job_id);
        let processed = storage::processed_path(&self.processed_dir, job_id);
        run_blocking(move || {
            let read_processed = || -> anyhow::Result<Option<SettledOutcome>> {
                Ok(storage::read_body_as::<OutcomeProbe>(&processed, true)?.map(|p| p.outcome()))
            };
            if let Some(outcome) = read_processed()? {
                return Ok(Some(outcome));
            }
            for path in [&incoming, &to_do, &parked] {
                if let Some(probe) = storage::read_body_as::<OutcomeProbe>(path, false)? {
                    return Ok(Some(probe.outcome()));
                }
            }
            if let Some(probe) = storage::read_body_as::<FinalizeProbe>(&to_finalize, false)? {
                return Ok(Some(probe.submission.outcome()));
            }
            read_processed()
        })
        .await
    }
}

#[async_trait]
impl SubmissionStore for LocalFsSubmissionStore {
    async fn write_incoming(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let incoming_dir = self.incoming_dir.clone();
        let job_id = job_id.clone();
        let body = body.clone();
        run_blocking(move || storage::write_submission(&incoming_dir, &job_id, &body)).await
    }

    async fn write_incoming_new(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        let incoming_dir = self.incoming_dir.clone();
        let job_id = job_id.clone();
        let body = body.clone();
        run_blocking(move || {
            // The scorer either misses the body or reads every byte of it.
            let path = storage::submission_path(&incoming_dir, &job_id);
            let body = serde_json::to_string_pretty(&body)?;
            storage::create_complete(&incoming_dir, &path, body.as_bytes())
        })
        .await
    }

    async fn write_processed(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let processed_dir = self.processed_dir.clone();
        let job_id = job_id.clone();
        let body = body.clone();
        run_blocking(move || storage::write_processed_direct(&processed_dir, &job_id, &body)).await
    }

    async fn get_submission(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
        let incoming_dir = self.incoming_dir.clone();
        let processed_dir = self.processed_dir.clone();
        let job_id = job_id.clone();
        run_blocking(move || {
            if let Some(body) = storage::read_submission_json(&incoming_dir, &job_id)? {
                return Ok(Some(SubmissionRecord {
                    job_id,
                    state: JobState::Incoming,
                    body,
                }));
            }
            if let Some(body) = storage::read_processed_json(&processed_dir, &job_id)? {
                return Ok(Some(SubmissionRecord {
                    job_id,
                    state: JobState::Processed,
                    body,
                }));
            }
            Ok(None)
        })
        .await
    }

    async fn list_incoming_after(
        &self,
        after: Option<&JobId>,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let incoming_dir = self.incoming_dir.clone();
        let after = after.cloned();
        run_blocking(move || storage::list_submission_job_ids(&incoming_dir, after.as_ref(), limit))
            .await
    }

    async fn mark_processed(&self, job_id: &JobId) -> anyhow::Result<()> {
        let incoming_dir = self.incoming_dir.clone();
        let processed_dir = self.processed_dir.clone();
        let job_id = job_id.clone();
        run_blocking(move || {
            storage::compress_submission_to_processed(&incoming_dir, &processed_dir, &job_id)
        })
        .await
    }

    async fn delete_incoming(&self, job_id: &JobId) -> anyhow::Result<()> {
        remove_file_idempotent(storage::submission_path(&self.incoming_dir, job_id)).await
    }

    async fn enqueue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let dir = self.stage_dir(stage);
        let job_id = job_id.clone();
        let body = body.clone();
        run_blocking(move || storage::write_submission(&dir, &job_id, &body)).await
    }

    async fn list_queue_after(
        &self,
        stage: ScoreQueueStage,
        after: Option<&JobId>,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let dir = self.stage_dir(stage);
        let after = after.cloned();
        run_blocking(move || storage::list_submission_job_ids(&dir, after.as_ref(), limit)).await
    }

    async fn read_queue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let dir = self.stage_dir(stage);
        let job_id = job_id.clone();
        run_blocking(move || storage::read_submission_json(&dir, &job_id)).await
    }

    async fn dequeue(&self, stage: ScoreQueueStage, job_id: &JobId) -> anyhow::Result<()> {
        remove_file_idempotent(storage::submission_path(&self.stage_dir(stage), job_id)).await
    }

    async fn find_job(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
        let incoming_dir = self.incoming_dir.clone();
        let processed_dir = self.processed_dir.clone();
        let job_id_owned = job_id.clone();
        let found = run_blocking(move || {
            let Some((body, state_label)) =
                storage::find_job(&incoming_dir, &processed_dir, &job_id_owned)?
            else {
                return Ok::<_, anyhow::Error>(None);
            };
            let state = if state_label == "processed" {
                JobState::Processed
            } else {
                JobState::Incoming
            };
            Ok(Some(SubmissionRecord {
                job_id: job_id_owned,
                state,
                body,
            }))
        })
        .await?;
        match found {
            Some(record) => Ok(Some(record)),
            None => self.find_in_score_queue(job_id).await,
        }
    }

    async fn has_record(&self, job_id: &JobId) -> anyhow::Result<bool> {
        // Check in the order that the trait documents.
        let paths = [
            storage::submission_path(&self.incoming_dir, job_id),
            storage::submission_path(&self.stage_dir(ScoreQueueStage::ToDo), job_id),
            storage::submission_path(&self.stage_dir(ScoreQueueStage::Parked), job_id),
            storage::submission_path(&self.stage_dir(ScoreQueueStage::ToFinalize), job_id),
            storage::processed_path(&self.processed_dir, job_id),
        ];
        // `try_exists` rather than `exists`: an IO failure must propagate, not
        // read as "no record".
        run_blocking(move || {
            for path in &paths {
                if path.try_exists()? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }

    async fn find_outcomes(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<HashMap<JobId, SettledOutcome>> {
        find_outcomes_per_job(
            job_ids,
            |job_id| async move { self.find_outcome(&job_id).await },
        )
        .await
    }

    async fn write_unverified(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let client_dir = self.unverified_dir.join(client_id.as_str());
        let job_id = job_id.clone();
        let body = body.clone();
        run_blocking(move || storage::write_submission(&client_dir, &job_id, &body)).await
    }

    async fn list_unverified_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>> {
        let unverified_dir = self.unverified_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || storage::list_unverified_client_dir(&unverified_dir, &client_id)).await
    }

    async fn delete_unverified(&self, client_id: &ClientId, job_id: &JobId) -> anyhow::Result<()> {
        let unverified_dir = self.unverified_dir.clone();
        let client_id = client_id.clone();
        let job_id = job_id.clone();
        run_blocking(move || {
            storage::delete_unverified_object(&unverified_dir, &client_id, &job_id)
        })
        .await
    }

    async fn delete_unverified_client(
        &self,
        client_id: &ClientId,
        dry_run: bool,
    ) -> anyhow::Result<usize> {
        let unverified_dir = self.unverified_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || {
            storage::delete_unverified_client_dir(&unverified_dir, &client_id, dry_run)
        })
        .await
    }

    async fn prune_unverified(
        &self,
        older_than: std::time::Duration,
        dry_run: bool,
    ) -> anyhow::Result<pipette_store::PruneSummary> {
        let unverified_dir = self.unverified_dir.clone();
        run_blocking(move || {
            let (deleted, kept) =
                storage::prune_unverified_dir(&unverified_dir, older_than, dry_run)?;
            Ok(pipette_store::PruneSummary { deleted, kept })
        })
        .await
    }

    async fn write_runtime_output(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        output: &RuntimeOutput,
    ) -> anyhow::Result<()> {
        let runtime_output_dir = self.runtime_output_dir.clone();
        let job_id = job_id.clone();
        let client_id = client_id.clone();
        let output = output.clone();
        run_blocking(move || {
            storage::write_runtime_output(&runtime_output_dir, &job_id, &client_id, &output)
        })
        .await
    }

    async fn list_runtime_output(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
        let runtime_output_dir = self.runtime_output_dir.clone();
        let job_id = job_id.clone();
        run_blocking(move || storage::list_runtime_output(&runtime_output_dir, &job_id)).await
    }
}

// ---------------------------------------------------------------------------
// WarehouseStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct LocalFsWarehouseStore {
    warehouse_dir: PathBuf,
    read_days: u32,
    max_rows_per_part: usize,
    writer_opts: WriterOpts,
}

impl LocalFsWarehouseStore {
    pub(crate) fn new(
        warehouse_dir: PathBuf,
        read_days: u32,
        max_rows_per_part: usize,
        writer_opts: WriterOpts,
    ) -> Self {
        Self {
            warehouse_dir,
            read_days,
            max_rows_per_part,
            writer_opts,
        }
    }
}

#[async_trait]
impl WarehouseStore for LocalFsWarehouseStore {
    async fn write_partition_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        day_key: &str,
        rows: &[MetricRow],
    ) -> anyhow::Result<()> {
        let warehouse_dir = self.warehouse_dir.clone();
        let benchmark_id = benchmark_id.clone();
        let client_id = client_id.clone();
        let day_key = day_key.to_string();
        let rows = rows.to_vec();
        let max_rows = self.max_rows_per_part;
        let writer_opts = self.writer_opts;
        run_blocking(move || {
            anyhow::ensure!(!rows.is_empty(), "cannot write empty metric rows");
            let partition_dir = warehouse::warehouse_day_partition_dir(
                &warehouse_dir,
                &benchmark_id,
                &client_id,
                &day_key,
            );
            warehouse::write_partition(writer_opts, &partition_dir, &rows, max_rows)
        })
        .await
    }

    async fn read_job_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        job_id: &JobId,
    ) -> anyhow::Result<Option<JobMetrics>> {
        let warehouse_dir = self.warehouse_dir.clone();
        let benchmark_id = benchmark_id.clone();
        let client_id = client_id.clone();
        let job_id = job_id.clone();
        let read_days = self.read_days;
        run_blocking(move || {
            warehouse::read_metrics_for_job(
                &warehouse_dir,
                &benchmark_id,
                &client_id,
                &job_id,
                read_days,
            )
        })
        .await
    }

    async fn for_each_metric_row(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a mut MetricRow) -> bool + Send),
    ) -> anyhow::Result<()> {
        // Sync I/O: maintenance-only, with no latency budget, and no moving a
        // borrowed `&mut dyn FnMut` into `spawn_blocking`.
        let mut stack = vec![self.warehouse_dir.clone()];
        while let Some(dir) = stack.pop() {
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            for entry in read {
                let entry = entry?;
                let path = entry.path();
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !ft.is_file() || path.extension().is_none_or(|e| e != "parquet") {
                    continue;
                }
                // An older binary staged under `.parquet`; reading such a file
                // as a part fails the whole pass. Drop once none can remain.
                if path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem.starts_with(".tmp-"))
                {
                    continue;
                }
                let mut rows = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
                if rows.is_empty() {
                    continue;
                }
                // `for_each`, not `any`: `any` short-circuits, and `f` must see
                // every row.
                let mut dirty = false;
                rows.iter_mut().for_each(|row| {
                    if f(row) {
                        dirty = true;
                    }
                });
                if !dirty {
                    continue;
                }
                let bytes = warehouse::rows_to_parquet_bytes(self.writer_opts, &rows)?;
                let parent = path.parent().ok_or_else(|| {
                    anyhow::anyhow!("parquet path missing parent: {}", path.display())
                })?;
                let tmp = warehouse::parquet_staging_path(parent);
                let guard = TmpFileGuard::new(tmp.clone());
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, &path)?;
                guard.disarm();
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EvalSampleResultStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct LocalFsEvalSampleResultStore {
    base_dir: PathBuf,
    writer_opts: WriterOpts,
}

impl LocalFsEvalSampleResultStore {
    pub(crate) fn new(base_dir: PathBuf, writer_opts: WriterOpts) -> Self {
        Self {
            base_dir,
            writer_opts,
        }
    }

    fn file_path(&self, job_id: &JobId) -> PathBuf {
        self.base_dir.join(format!("{job_id}.parquet"))
    }
}

#[async_trait]
impl EvalSampleResultStore for LocalFsEvalSampleResultStore {
    async fn write(&self, job_id: &JobId, rows: &[EvalSampleResult]) -> anyhow::Result<()> {
        let path = self.file_path(job_id);
        let base_dir = self.base_dir.clone();
        let row_count = rows.len();
        let rows = rows.to_vec();
        let writer_opts = self.writer_opts;
        run_blocking(move || {
            std::fs::create_dir_all(&base_dir)?;
            if rows.is_empty() {
                anyhow::bail!("refusing to write eval sample results with no rows");
            }
            let replacing = path.exists();
            // Temp file then rename; the guard reaps the temp on an early error.
            let tmp_path = base_dir.join(format!(".tmp-{}.parquet", uuid::Uuid::new_v4()));
            let guard = TmpFileGuard::new(tmp_path.clone());
            tracing::debug!(
                tmp_path = %tmp_path.display(),
                rows = row_count,
                "writing eval sample results to temp file"
            );
            eval_sample_result::write_parquet(writer_opts, &tmp_path, &rows)?;
            if replacing {
                tracing::info!(
                    path = %path.display(),
                    "replacing existing eval sample results file"
                );
            }
            std::fs::rename(&tmp_path, &path)?;
            guard.disarm();
            tracing::info!(
                path = %path.display(),
                rows = row_count,
                "eval sample results created"
            );
            Ok(())
        })
        .await
    }

    async fn read(&self, job_id: &JobId) -> anyhow::Result<Option<Vec<EvalSampleResult>>> {
        let path = self.file_path(job_id);
        run_blocking(move || {
            let result = eval_sample_result::read_parquet(&path)?;
            match &result {
                Some(rows) => tracing::debug!(
                    path = %path.display(),
                    rows = rows.len(),
                    "read eval sample results"
                ),
                None => tracing::debug!(
                    path = %path.display(),
                    "eval sample results file not found"
                ),
            }
            Ok(result)
        })
        .await
    }

    async fn list_job_ids(&self) -> anyhow::Result<Vec<JobId>> {
        let base_dir = self.base_dir.clone();
        run_blocking(move || {
            let read = match std::fs::read_dir(&base_dir) {
                Ok(read) => read,
                // Created on first write; absent means no results yet.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            let mut job_ids = Vec::new();
            for entry in read {
                let path = entry?.path();
                // Only `{job_id}.parquet`, not a `.tmp-*.parquet` an interrupted
                // write left, nor a stem that is no job id.
                if path.extension().is_none_or(|ext| ext != "parquet") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if stem.starts_with(".tmp-") {
                    continue;
                }
                match JobId::try_new(stem) {
                    Ok(job_id) => job_ids.push(job_id),
                    Err(e) => tracing::warn!(
                        file = %path.display(),
                        error = %e,
                        "skipping eval sample results file with non-job-id name"
                    ),
                }
            }
            Ok(job_ids)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// TodoStore
// ---------------------------------------------------------------------------

pub struct LocalFsTodoStore {
    avail_dir: PathBuf,
    leased_dir: PathBuf,
    denied_dir: PathBuf,
    eligible_clients_dir: PathBuf,
    pending_reindex_dir: PathBuf,
    pending_reindex_jobs_dir: PathBuf,
    lost_dir: PathBuf,
    tmp_dir: PathBuf,
    suspended_dir: PathBuf,
    liveness_dir: PathBuf,
    indexed_path: PathBuf,
    workers_path: PathBuf,
    gc_candidates_path: PathBuf,
    backlog_path: PathBuf,
    campaign_backlog_path: PathBuf,
}

impl LocalFsTodoStore {
    pub fn new(todo_dir: PathBuf) -> Self {
        Self {
            avail_dir: todo_dir.join("avail"),
            leased_dir: todo_dir.join("leased"),
            denied_dir: todo_dir.join("denied"),
            eligible_clients_dir: todo_dir.join("eligible").join("clients"),
            pending_reindex_dir: todo_dir.join("pending-reindex"),
            pending_reindex_jobs_dir: todo_dir.join("pending-reindex-jobs"),
            lost_dir: todo_dir.join("lost"),
            tmp_dir: todo_dir.join("tmp"),
            suspended_dir: todo_dir.join("suspended"),
            liveness_dir: todo_dir.join("liveness"),
            indexed_path: todo_dir.join(".eligible-indexed"),
            workers_path: todo_dir.join(".eligible-workers"),
            gc_candidates_path: todo_dir.join(".gc-candidates"),
            backlog_path: todo_dir.join(".backlog"),
            campaign_backlog_path: todo_dir.join(".backlog-campaigns"),
        }
    }

    /// Path to a client's `suspended/{client_id}.json` marker.
    fn suspension_path(&self, client_id: &ClientId) -> PathBuf {
        self.suspended_dir.join(format!("{}.json", client_id))
    }

    /// Path to a client's `liveness/{client_id}` marker.
    fn liveness_path(&self, client_id: &ClientId) -> PathBuf {
        self.liveness_dir.join(client_id.as_str())
    }

    /// Path to a job's `denied/{job_id}.{client_id}` marker.
    fn denied_path(&self, job_id: &JobId, client_id: &ClientId) -> PathBuf {
        self.denied_dir
            .join(denied_marker_filename(job_id, client_id))
    }

    /// Path to a job's `avail/{job_id}/` directory, which holds its one entry.
    /// [`pipette_core::todo_filename::avail_filename`] gives the layout.
    fn avail_job_dir(&self, job_id: &JobId) -> PathBuf {
        self.avail_dir.join(job_id.as_str())
    }
}

#[async_trait]
impl TodoStore for LocalFsTodoStore {
    async fn list_avail(&self) -> anyhow::Result<Vec<String>> {
        let avail_dir = self.avail_dir.clone();
        run_blocking(move || {
            // Every read error propagates: a job missed here reads as "not in
            // avail/", and the GC deletes its markers and reindexes it as new.
            // Non-UTF-8 and non-`.json` names are cruft and dropped.
            let mut names: Vec<String> =
                std::fs::read_dir(&avail_dir)?.try_fold(Vec::new(), |mut names, entry| {
                    let entry = entry?;
                    let Ok(job) = entry.file_name().into_string() else {
                        return anyhow::Ok(names);
                    };
                    if !entry.file_type()?.is_dir() {
                        // A file directly under `avail/` is cruft or an older
                        // layout's entry; report it so the sweep's guard sees it.
                        names.extend(json_name(&entry));
                        return anyhow::Ok(names);
                    }
                    // A job directory leaves with the job's last entry, so one
                    // that vanishes mid-walk holds nothing.
                    let Some(leaves) = vanished_is_absent(std::fs::read_dir(entry.path()))? else {
                        return anyhow::Ok(names);
                    };
                    leaves.into_iter().try_fold(names, |mut names, leaf| {
                        names.extend(json_name(&leaf?).map(|n| format!("{job}/{n}")));
                        anyhow::Ok(names)
                    })
                })?;
            names.sort_unstable();
            Ok(names)
        })
        .await
    }

    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let path = self.avail_dir.join(avail_filename(job_id, expires_at));
        run_blocking(move || match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        let job_dir = self.avail_job_dir(job_id);
        let path = self.avail_dir.join(avail_filename(job_id, expires_at));
        run_blocking(move || {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                // Idempotent: the entry is already gone.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            drop_empty_dir(&job_dir);
            Ok(())
        })
        .await
    }

    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let job_dir = self.avail_job_dir(job_id);
        run_blocking(move || {
            // A read error must fail the delete: read as absent, it would report
            // the job torn down while its `avail/` file stays.
            let Some(mut entries) = vanished_is_absent(std::fs::read_dir(&job_dir))? else {
                return Ok(());
            };
            entries.try_for_each(|entry| -> anyhow::Result<()> {
                let entry = entry?;
                if json_name(&entry).is_some() {
                    std::fs::remove_file(entry.path())?;
                }
                Ok(())
            })?;
            drop_empty_dir(&job_dir);
            Ok(())
        })
        .await
    }

    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let job_dir = self.avail_job_dir(job_id);
        run_blocking(move || {
            // A read error propagates: an unreadable dir read as `None` would
            // skip a legitimate escalation.
            let Some(entries) = vanished_is_absent(std::fs::read_dir(&job_dir))? else {
                return Ok(None);
            };
            entries
                .map(|entry| -> anyhow::Result<Option<serde_json::Value>> {
                    let entry = entry?;
                    json_name(&entry)
                        .map(|_| {
                            Ok(serde_json::from_str(&std::fs::read_to_string(
                                entry.path(),
                            )?)?)
                        })
                        .transpose()
                })
                // The first JSON entry decides; a read error ahead of it still
                // propagates.
                .filter_map(|r| r.transpose())
                .next()
                .transpose()
        })
        .await
    }

    async fn avail_key_for_job(&self, job_id: &JobId) -> anyhow::Result<Option<String>> {
        let job_dir = self.avail_job_dir(job_id);
        let job_id = job_id.clone();
        run_blocking(move || {
            // A read error propagates: an empty answer sends a merge target to
            // the dead-check and a claim to the next candidate.
            let Some(entries) = vanished_is_absent(std::fs::read_dir(&job_dir))? else {
                return Ok(None);
            };
            entries
                .map(|entry| -> anyhow::Result<Option<String>> { Ok(json_name(&entry?)) })
                .filter_map(|r| r.transpose())
                // The first entry decides the answer, and the job segment goes
                // back in front of it to make the key the filename parser reads.
                .next()
                .transpose()
                .map(|name| name.map(|name| format!("{job_id}/{name}")))
        })
        .await
    }

    async fn redate_avail(
        &self,
        job_id: &JobId,
        from: ExpiresAt,
        to: ExpiresAt,
    ) -> anyhow::Result<RedateResult> {
        let job_dir = self.avail_job_dir(job_id);
        let src = self.avail_dir.join(avail_filename(job_id, from));
        let dst = self.avail_dir.join(avail_filename(job_id, to));
        run_blocking(move || {
            // Both names sit in the job's own directory. Create it first, so a
            // missing directory cannot masquerade as the `Gone` below.
            std::fs::create_dir_all(&job_dir)?;
            // Same-directory `rename` is atomic, which `claim_job` relies on. A
            // missing source is `Gone`: a claim took the entry first.
            match std::fs::rename(&src, &dst) {
                Ok(()) => Ok(RedateResult::Redated),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RedateResult::Gone),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        let leased_dir = self.leased_dir.clone();
        run_blocking(move || {
            // Keys are `{client_id}/{leaf}`. Every read error but a vanished
            // partition propagates: a lease missed here reads as "not leased",
            // and the GC deletes its markers.
            let keys = std::fs::read_dir(&leased_dir)?
                .map(|client_entry| -> anyhow::Result<Vec<String>> {
                    let client_entry = client_entry?;
                    if !client_entry.file_type()?.is_dir() {
                        return Ok(Vec::new());
                    }
                    let client_name = client_entry.file_name().to_string_lossy().into_owned();
                    match std::fs::read_dir(client_entry.path()) {
                        Ok(leaves) => leaves
                            .map(|leaf_entry| -> anyhow::Result<Option<String>> {
                                let leaf = leaf_entry?.file_name().to_string_lossy().into_owned();
                                Ok(is_json_name(&leaf).then(|| format!("{client_name}/{leaf}")))
                            })
                            .filter_map(|r| r.transpose())
                            .collect::<anyhow::Result<Vec<_>>>(),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                        Err(e) => Err(anyhow::Error::from(e)),
                    }
                })
                .collect::<anyhow::Result<Vec<Vec<String>>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<String>>();
            Ok(keys)
        })
        .await
    }

    async fn get_leased(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let path = self
            .leased_dir
            .join(leased_key(job_id, expires_at, client_id, lease_expiry));
        run_blocking(move || match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        let client_dir = self.leased_dir.join(client_id.as_str());
        let client_name = client_id.as_str().to_owned();
        run_blocking(move || match std::fs::read_dir(&client_dir) {
            Ok(entries) => Ok(entries
                .filter_map(|e| readable_entry(&client_dir, e))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| is_json_name(n))
                .map(|leaf| format!("{client_name}/{leaf}"))
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult> {
        let leased_dir = self.leased_dir.clone();
        let job_id = job_id.clone();
        let client_id = client_id.clone();
        run_blocking(move || {
            let job_prefix = format!("{}.", job_id);

            // Renewal needs the lease to exist, not to be unexpired: only the
            // recycler ends a lease, atomically with this rename. Do not add an
            // expiry check; it would race the recycler.
            let own_dir = leased_dir.join(client_id.as_str());
            let own_leaf = match std::fs::read_dir(&own_dir) {
                Ok(entries) => entries
                    .filter_map(|e| readable_entry(&own_dir, e))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.starts_with(&job_prefix))
                    .find_map(|n| {
                        parse_leased_key(&format!("{client_id}/{n}"))
                            .ok()
                            .map(|lease| (n, lease.expires_at))
                    }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };

            if let Some((leaf, expires_at)) = own_leaf {
                let src = own_dir.join(&leaf);
                let dst = leased_dir.join(leased_key(&job_id, expires_at, &client_id, new_expiry));
                match std::fs::rename(&src, &dst) {
                    Ok(()) => return Ok(RenewLeaseResult::Renewed(expires_at)),
                    // Lost a race with a recycle; fall through to the check below.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }

            // No lease of ours. Finding the job in another partition is
            // conclusive (WrongClient, 409); not finding it is only conclusive
            // if every partition was read. So only `NotFound` reads as "not
            // here"; any other error propagates rather than fake a 404. Not a
            // plain `.any()` over `.flatten()`: that swallows the errors.
            let wrong_client = std::fs::read_dir(&leased_dir)?
                .filter_map(|e| readable_entry(&leased_dir, e))
                .filter(|entry| entry_is(entry, std::fs::FileType::is_dir))
                // Our own partition was checked above.
                .filter(|entry| entry.file_name().to_string_lossy() != client_id.as_str())
                .try_fold(false, |found, entry| -> anyhow::Result<bool> {
                    // Already found a holder: skip the remaining reads.
                    if found {
                        return Ok(true);
                    }
                    let partition = entry.path();
                    let held = match std::fs::read_dir(&partition) {
                        Ok(leaves) => leaves
                            .filter_map(|e| readable_entry(&partition, e))
                            .filter(|e| entry_is(e, std::fs::FileType::is_file))
                            .any(|e| e.file_name().to_string_lossy().starts_with(&job_prefix)),
                        // Partition vanished → that client holds nothing.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                        Err(e) => return Err(e.into()),
                    };
                    Ok(held)
                })?;

            Ok(if wrong_client {
                RenewLeaseResult::WrongClient
            } else {
                RenewLeaseResult::NotFound
            })
        })
        .await
    }

    async fn delete_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        remove_file_idempotent(
            self.leased_dir
                .join(leased_key(job_id, expires_at, client_id, expiry)),
        )
        .await
    }

    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        let job_dir = self.avail_job_dir(job_id);
        let src = self.avail_dir.join(avail_filename(job_id, expires_at));
        let dst = self
            .leased_dir
            .join(leased_key(job_id, expires_at, client_id, lease_expiry));
        run_blocking(move || {
            // Ensure the client partition exists before renaming into it. Create
            // it first so a missing dir doesn't masquerade as a lost claim race.
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::rename(&src, &dst) {
                Ok(()) => {
                    let body: serde_json::Value =
                        serde_json::from_str(&std::fs::read_to_string(&dst)?)?;
                    drop_empty_dir(&job_dir);
                    Ok(ClaimResult::Claimed(body))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClaimResult::Gone),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn recycle_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        let src = self
            .leased_dir
            .join(leased_key(job_id, expires_at, client_id, lease_expiry));
        let dst = self.avail_dir.join(avail_filename(job_id, expires_at));
        let job_dir = self.avail_job_dir(job_id);
        run_blocking(move || {
            // `rename` reports a missing destination dir like a missing source,
            // so a lost `avail/` would read as `Gone` and strand every lease.
            std::fs::create_dir_all(&job_dir)?;
            // A missing source is `Gone`: the lease vanishes when the holder's
            // terminal submission (or another recycler) beats us to it.
            match std::fs::rename(&src, &dst) {
                Ok(()) => Ok(RecycleResult::Recycled),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RecycleResult::Gone),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn write_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        record: &DenialRecord,
    ) -> anyhow::Result<()> {
        let dir = self.denied_dir.clone();
        let path = self.denied_path(job_id, client_id);
        let body = record.to_bytes()?;
        // A concurrent `get_denied` reads the whole record or no marker.
        // `list_all_denied` skips the `.tmp-` staging name.
        run_blocking(move || {
            storage::replace_atomically(&dir, &path, |tmp| std::fs::write(tmp, body))
        })
        .await
    }

    async fn get_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<DenialRecord>> {
        let path = self.denied_path(job_id, client_id);
        run_blocking(move || match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(DenialRecord::from_bytes(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn is_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<bool> {
        let path = self.denied_path(job_id, client_id);
        // `try_exists` rather than `exists`: a permission or IO failure must
        // propagate, not read as "not denied".
        run_blocking(move || Ok(path.try_exists()?)).await
    }

    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let denied_dir = self.denied_dir.clone();
        let prefix = denied_job_prefix(job_id);
        run_blocking(move || {
            for entry in std::fs::read_dir(&denied_dir)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    std::fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        remove_file_idempotent(self.denied_path(job_id, client_id)).await
    }

    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        let denied_dir = self.denied_dir.clone();
        run_blocking(move || {
            let entries = match std::fs::read_dir(&denied_dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(e.into()),
            };
            // Malformed markers are logged and skipped: harmless un-GC'd, as
            // the sweep only deletes. A `.tmp-` name is a `write_denied` in
            // progress.
            let markers = entries
                .filter_map(|e| readable_entry(&denied_dir, e))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with(".tmp-"))
                .filter_map(|name| {
                    parse_denied_marker(&name)
                        .inspect_err(|e| {
                            tracing::warn!(marker = %name, error = %e, "skipping malformed denied marker");
                        })
                        .ok()
                })
                .collect();
            Ok(markers)
        })
        .await
    }

    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
        let client_dir = self.eligible_clients_dir.join(client_id.as_str());
        run_blocking(move || match std::fs::read_dir(&client_dir) {
            Ok(entries) => entries
                .map(|e| parse_eligible_filename(e?.file_name().to_string_lossy().as_ref()))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let client_dir = self.eligible_clients_dir.join(client_id.as_str());
        let path = client_dir.join(eligible_filename(job_id, expires_at));
        run_blocking(move || {
            std::fs::create_dir_all(&client_dir)?;
            Ok(std::fs::write(&path, b"")?)
        })
        .await
    }

    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let client_dir = self.eligible_clients_dir.join(client_id.as_str());
        run_blocking(move || match std::fs::remove_dir_all(&client_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        remove_file_idempotent(
            self.eligible_clients_dir
                .join(client_id.as_str())
                .join(eligible_filename(job_id, expires_at)),
        )
        .await
    }

    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        let eligible_clients_dir = self.eligible_clients_dir.clone();
        run_blocking(move || {
            let client_entries = match std::fs::read_dir(&eligible_clients_dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(e.into()),
            };
            // Tolerates concurrent mutation (`clients delete` mid-sweep).
            // Malformed names are logged and skipped: the sweep only deletes.
            let mut result = Vec::new();
            for client_entry in
                client_entries.filter_map(|e| readable_entry(&eligible_clients_dir, e))
            {
                if !entry_is(&client_entry, std::fs::FileType::is_dir) {
                    continue;
                }
                let dir_name = client_entry.file_name().to_string_lossy().into_owned();
                let client_id = match id_from_key(dir_name.as_str(), ClientId::try_new) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(partition = %dir_name, error = %e, "skipping eligible partition with invalid client_id");
                        continue;
                    }
                };
                let client_dir = client_entry.path();
                let job_entries = match std::fs::read_dir(&client_dir) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                for job_entry in job_entries.filter_map(|e| readable_entry(&client_dir, e)) {
                    let name = job_entry.file_name().to_string_lossy().into_owned();
                    match parse_eligible_filename(&name) {
                        Ok((job_id, expires_at)) => {
                            result.push((client_id.clone(), job_id, expires_at))
                        }
                        Err(e) => {
                            tracing::warn!(marker = %name, client_id = %client_id, error = %e, "skipping malformed eligible marker")
                        }
                    }
                }
            }
            Ok(result)
        })
        .await
    }

    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let path =
            self.pending_reindex_dir
                .join(pipette_core::todo_filename::pending_reindex_filename(
                    client_id,
                ));
        run_blocking(move || Ok(std::fs::write(&path, b"")?)).await
    }

    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        let pending_reindex_dir = self.pending_reindex_dir.clone();
        run_blocking(move || {
            // Read errors propagate (the gate must not lose a flag); an
            // unparseable name is cruft, warned and skipped.
            std::fs::read_dir(&pending_reindex_dir)?
                .map(|e| anyhow::Ok(e?.file_name().to_string_lossy().into_owned()))
                .filter_map(|r| {
                    r.map(
                        |name| match pipette_core::todo_filename::parse_pending_reindex_filename(
                            &name,
                        ) {
                            Ok(client_id) => Some((client_id, name)),
                            Err(_) => {
                                tracing::warn!(
                                    key = %name,
                                    "skipping unparseable pending-reindex flag"
                                );
                                None
                            }
                        },
                    )
                    .transpose()
                })
                .collect()
        })
        .await
    }

    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()> {
        remove_file_idempotent(self.pending_reindex_dir.join(key)).await
    }

    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        let pending_reindex_dir = self.pending_reindex_dir.clone();
        let client_id = client_id.clone();
        // Entry read errors propagate, per the trait contract — the gate must
        // not read "couldn't check" as "not flagged".
        run_blocking(move || {
            std::fs::read_dir(&pending_reindex_dir)?.try_fold(false, |found, e| {
                let name = e?.file_name().to_string_lossy().into_owned();
                anyhow::Ok(
                    found
                        || matches!(
                            pipette_core::todo_filename::parse_pending_reindex_filename(&name),
                            Ok(flagged) if flagged == client_id
                        ),
                )
            })
        })
        .await
    }

    async fn write_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let path = self.pending_reindex_jobs_dir.join(job_id.as_str());
        run_blocking(move || Ok(std::fs::write(&path, b"")?)).await
    }

    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        let pending_reindex_jobs_dir = self.pending_reindex_jobs_dir.clone();
        run_blocking(move || {
            // Read errors propagate (a lost flag is lost reindex debt); an
            // unparseable name is warned and skipped rather than wedge every run.
            std::fs::read_dir(&pending_reindex_jobs_dir)?
                .map(|e| anyhow::Ok(e?.file_name().to_string_lossy().into_owned()))
                .filter_map(|r| {
                    r.map(|name| match JobId::try_new(&name) {
                        Ok(job_id) => Some(job_id),
                        Err(_) => {
                            tracing::warn!(
                                key = %name,
                                "skipping unparseable pending-reindex-jobs entry"
                            );
                            None
                        }
                    })
                    .transpose()
                })
                .collect()
        })
        .await
    }

    async fn delete_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        remove_file_idempotent(self.pending_reindex_jobs_dir.join(job_id.as_str())).await
    }

    async fn write_lost_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let job_dir = self.lost_dir.join(job_id.as_str());
        let path = job_dir.join(lost_lease_filename(client_id, lease_expiry));
        run_blocking(move || {
            std::fs::create_dir_all(&job_dir)?;
            Ok(std::fs::write(&path, b"")?)
        })
        .await
    }

    async fn list_lost_leases(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        let job_dir = self.lost_dir.join(job_id.as_str());
        run_blocking(move || {
            let Some(entries) = vanished_is_absent(std::fs::read_dir(&job_dir))? else {
                return Ok(Vec::new());
            };
            let mut losses = entries
                .map(|e| anyhow::Ok(e?.file_name().to_string_lossy().into_owned()))
                .filter_map(|r| {
                    r.map(|name| {
                        parse_lost_lease_filename(&name)
                            .inspect_err(|e| {
                                tracing::warn!(key = %name, error = %e, "skipping unparseable lost-lease marker");
                            })
                            .ok()
                    })
                    .transpose()
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            losses.sort_by_key(|(_, expiry)| *expiry);
            Ok(losses)
        })
        .await
    }

    async fn delete_lost_leases_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let job_dir = self.lost_dir.join(job_id.as_str());
        run_blocking(move || match std::fs::remove_dir_all(&job_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        let lost_dir = self.lost_dir.clone();
        run_blocking(move || {
            let Some(entries) = vanished_is_absent(std::fs::read_dir(&lost_dir))? else {
                return Ok(Vec::new());
            };
            entries
                .map(|e| anyhow::Ok(e?.file_name().to_string_lossy().into_owned()))
                .filter_map(|r| {
                    r.map(|name| {
                        JobId::try_new(&name)
                            .inspect_err(|_| {
                                tracing::warn!(key = %name, "skipping unparseable lost/ entry");
                            })
                            .ok()
                    })
                    .transpose()
                })
                .collect()
        })
        .await
    }

    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let path = self.tmp_dir.join(tmp_filename(job_id));
        let bytes = serde_json::to_vec(body)?;
        run_blocking(move || Ok(std::fs::write(&path, bytes)?)).await
    }

    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        // Same-filesystem `rename` is atomic: the job appears in `avail/` whole
        // or not at all. `avail/` is created first, so `NotFound` names the source.
        let src = self.tmp_dir.join(tmp_filename(job_id));
        let dst = self.avail_dir.join(avail_filename(job_id, expires_at));
        let job_dir = self.avail_job_dir(job_id);
        run_blocking(move || {
            std::fs::create_dir_all(&job_dir)?;
            std::fs::rename(&src, &dst).map_err(|e| {
                anyhow::anyhow!(
                    "promote_avail rename {} -> {}: {e}",
                    src.display(),
                    dst.display()
                )
            })
        })
        .await
    }

    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>> {
        let tmp_dir = self.tmp_dir.clone();
        run_blocking(move || {
            let now = std::time::SystemTime::now();
            let mut stale = Vec::new();
            for entry in std::fs::read_dir(&tmp_dir)? {
                let entry = entry?;
                let modified = entry.metadata()?.modified()?;
                if now.duration_since(modified).unwrap_or_default() >= age {
                    stale.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
            Ok(stale)
        })
        .await
    }

    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()> {
        remove_file_idempotent(self.tmp_dir.join(key)).await
    }

    async fn read_eligible_indexed(&self) -> anyhow::Result<HashSet<String>> {
        read_key_set(self.indexed_path.clone(), "eligible-indexed").await
    }

    async fn write_eligible_indexed(&self, indexed: &HashSet<String>) -> anyhow::Result<()> {
        write_key_set(self.indexed_path.clone(), indexed).await
    }

    async fn read_eligible_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
        read_key_set(self.workers_path.clone(), "eligible-workers").await
    }

    async fn write_eligible_workers(&self, workers: &HashSet<ClientId>) -> anyhow::Result<()> {
        write_key_set(self.workers_path.clone(), workers).await
    }

    async fn read_backlog(&self) -> anyhow::Result<Option<BacklogSnapshot>> {
        let path = self.backlog_path.clone();
        run_blocking(move || {
            Ok(vanished_is_absent(std::fs::read(&path))?
                .and_then(|bytes| pipette_store::parse_snapshot(&bytes, "todo/.backlog")))
        })
        .await
    }

    async fn write_backlog(&self, snapshot: &BacklogSnapshot) -> anyhow::Result<()> {
        replace_snapshot(self.backlog_path.clone(), serde_json::to_vec(snapshot)?).await
    }

    async fn read_campaign_backlog(&self) -> anyhow::Result<Option<CampaignBacklogSnapshot>> {
        let path = self.campaign_backlog_path.clone();
        run_blocking(move || {
            Ok(vanished_is_absent(std::fs::read(&path))?
                .and_then(|bytes| pipette_store::parse_snapshot(&bytes, "todo/.backlog-campaigns")))
        })
        .await
    }

    async fn write_campaign_backlog(
        &self,
        snapshot: &CampaignBacklogSnapshot,
    ) -> anyhow::Result<()> {
        replace_snapshot(
            self.campaign_backlog_path.clone(),
            serde_json::to_vec(snapshot)?,
        )
        .await
    }

    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        read_key_set(self.gc_candidates_path.clone(), "gc-candidates").await
    }

    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
        write_key_set(self.gc_candidates_path.clone(), candidates).await
    }

    async fn write_suspension(
        &self,
        client_id: &ClientId,
        suspended_at: DateTime<Utc>,
        conflicting_job_id: &JobId,
    ) -> anyhow::Result<()> {
        let path = self.suspension_path(client_id);
        let record = SuspensionRecord {
            suspended_at,
            conflicting_job_id: conflicting_job_id.clone(),
        };
        let body = serde_json::to_string(&record)?;
        run_blocking(move || Ok(std::fs::write(&path, body.as_bytes())?)).await
    }

    async fn read_suspension(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<SuspensionRecord>> {
        let path = self.suspension_path(client_id);
        run_blocking(move || match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn delete_suspension(&self, client_id: &ClientId) -> anyhow::Result<()> {
        remove_file_idempotent(self.suspension_path(client_id)).await
    }

    async fn list_suspended_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        let suspended_dir = self.suspended_dir.clone();
        run_blocking(move || {
            if !suspended_dir.exists() {
                return Ok(Vec::new());
            }
            let mut ids: Vec<ClientId> = std::fs::read_dir(&suspended_dir)?
                .filter_map(|entry| readable_entry(&suspended_dir, entry))
                .filter_map(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .and_then(|name| pipette_store::flat_keyspace_id(name, ClientId::try_new))
                })
                .collect();
            ids.sort();
            Ok(ids)
        })
        .await
    }

    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        let suspended_dir = self.suspended_dir.clone();
        run_blocking(move || {
            // A malformed marker is logged and skipped: one bad file must not
            // hide every other suspended client.
            let mut result = Vec::new();
            for entry in
                std::fs::read_dir(&suspended_dir)?.filter_map(|e| readable_entry(&suspended_dir, e))
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id_str) = name.strip_suffix(".json") else {
                    continue;
                };
                let client_id = match id_from_key(id_str, ClientId::try_new) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(marker = %name, error = %e, "skipping suspension marker with invalid client_id");
                        continue;
                    }
                };
                let record = || -> anyhow::Result<SuspensionRecord> {
                    Ok(serde_json::from_str(&std::fs::read_to_string(entry.path())?)?)
                };
                match record() {
                    Ok(record) => result.push((client_id, record)),
                    Err(e) => {
                        tracing::warn!(client_id = %client_id, error = %e, "skipping unreadable suspension record")
                    }
                }
            }
            Ok(result)
        })
        .await
    }

    async fn stamp_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let path = self.liveness_path(client_id);
        // Truncating a file that is already empty need not move its `mtime`, so
        // the stamp sets the time itself.
        run_blocking(move || {
            Ok(std::fs::File::create(&path)?.set_modified(std::time::SystemTime::now())?)
        })
        .await
    }

    async fn read_liveness(&self, client_id: &ClientId) -> anyhow::Result<Option<DateTime<Utc>>> {
        let path = self.liveness_path(client_id);
        run_blocking(move || {
            Ok(
                vanished_is_absent(std::fs::metadata(&path).and_then(|m| m.modified()))?
                    .map(DateTime::<Utc>::from),
            )
        })
        .await
    }

    async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        let dir = self.liveness_dir.clone();
        run_blocking(move || {
            let Some(entries) = vanished_is_absent(std::fs::read_dir(&dir))? else {
                return Ok(Vec::new());
            };
            // An unreadable marker is logged and skipped; the orphan sweep keeps
            // its device until a later run.
            Ok(entries
                .filter_map(|entry| readable_entry(&dir, entry))
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let Ok(client_id) = id_from_key(&name, ClientId::try_new) else {
                        tracing::warn!(key = %name, "skipping unparseable liveness marker");
                        return None;
                    };
                    match entry.metadata().and_then(|m| m.modified()) {
                        Ok(modified) => Some((client_id, modified.into())),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => {
                            tracing::warn!(
                                key = %name,
                                error = %e,
                                "skipping liveness marker with an unreadable modification time"
                            );
                            None
                        }
                    }
                })
                .collect())
        })
        .await
    }

    async fn delete_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        remove_file_idempotent(self.liveness_path(client_id)).await
    }

    /// Always atomic: same-filesystem `rename` is atomic on POSIX.
    async fn validate_backend(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use chrono::Utc;
    use rstest::rstest;

    use pipette_core::client::Client;
    use pipette_core::types::{DeriveFromKey, Mint};
    use pipette_plan_types::{BenchmarkId, ClientId, JobId};
    use pipette_store::ClientStore;
    use pipette_store_testkit::TEST_LIST_LIMIT;

    use crate::*;

    /// The contracts' raw-write hook: `key` relative to the store's root.
    pub(crate) fn write_raw_under(
        root: &Path,
    ) -> impl Fn(String, Vec<u8>) -> std::future::Ready<anyhow::Result<()>> + '_ {
        move |key, bytes| std::future::ready(write_creating_parents(&root.join(key), bytes))
    }

    fn write_creating_parents(path: &Path, bytes: Vec<u8>) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// A malformed entry must not cost the deployment its whole catalog.
    #[tokio::test]
    async fn catalog_store_skips_an_unreadable_entry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let benchmarks = dir.path().join("benchmarks");
        std::fs::create_dir_all(&benchmarks)?;
        std::fs::write(
            benchmarks.join("prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )?;
        std::fs::write(
            benchmarks.join("bad_unknown_key.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 1\nnope = 1",
        )?;

        let store = LocalFsCatalogStore::new(dir.path());
        let catalog = store.load_catalog().await?;
        assert_eq!(catalog.len(), 1);
        assert!(catalog.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_org_catalog_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_org_catalog_store_roundtrip(&LocalFsCatalogStore::new(
            dir.path(),
        ))
        .await
    }

    #[tokio::test]
    async fn test_localfs_org_benchmark_listing_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("org-benchmarks");
        pipette_store_testkit::assert_org_benchmark_listing(
            &LocalFsCatalogStore::new(dir.path()),
            |org_id, id, body| {
                let org_dir = root.join(org_id.as_str());
                async move {
                    std::fs::create_dir_all(&org_dir)?;
                    Ok(std::fs::write(org_dir.join(format!("{id}.toml")), body)?)
                }
            },
        )
        .await
    }

    #[tokio::test]
    async fn test_localfs_org_benchmark_ids_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_org_benchmark_ids_span_every_org(&LocalFsCatalogStore::new(
            dir.path(),
        ))
        .await
    }

    /// An abandoned `.tmp-` file, a stray file and a non-org directory are
    /// passed over.
    #[tokio::test]
    async fn test_localfs_org_benchmark_ids_skip_what_is_not_an_entry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsCatalogStore::new(dir.path());
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        store.put_org_benchmark(&acme, &entry).await?;
        let root = dir.path().join("org-benchmarks");
        std::fs::write(root.join(acme.as_str()).join(".tmp-abandoned"), "")?;
        std::fs::write(root.join("README.toml"), "")?;
        std::fs::create_dir_all(root.join("not-an-org"))?;
        std::fs::write(root.join("not-an-org").join("x.toml"), "")?;

        assert_eq!(
            store.list_org_benchmark_ids().await?,
            vec![(acme, entry.benchmark_id().clone())]
        );
        Ok(())
    }

    /// An org's entries sit beside `benchmarks/`, never inside it, so the
    /// public loader cannot pick them up.
    #[tokio::test]
    async fn test_localfs_org_catalog_layout() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let benchmarks = dir.path().join("benchmarks");
        std::fs::create_dir_all(&benchmarks)?;
        std::fs::write(
            benchmarks.join("prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )?;
        let store = LocalFsCatalogStore::new(dir.path());
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        store.put_org_benchmark(&acme, &entry).await?;

        let path = dir
            .path()
            .join("org-benchmarks")
            .join(acme.as_str())
            .join(format!("{}.toml", entry.benchmark_id()));
        assert!(path.is_file(), "{}", path.display());
        assert_eq!(
            std::fs::read_to_string(&path)?,
            entry.to_toml()?,
            "the file is the definition alone"
        );
        let public = store.load_catalog().await?;
        assert_eq!(public.len(), 1);
        assert!(!public.contains_key(entry.benchmark_id()));
        Ok(())
    }

    /// An unreadable entry is skipped by a listing and named by a read by id;
    /// a `.tmp-` file has no `.toml` extension, so it is no entry.
    #[tokio::test]
    async fn test_localfs_org_catalog_unreadable_entry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsCatalogStore::new(dir.path());
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        store.put_org_benchmark(&acme, &entry).await?;
        let org_dir = dir.path().join("org-benchmarks").join(acme.as_str());
        std::fs::write(org_dir.join("broken.toml"), "benchmark_type = \"nope\"")?;
        std::fs::write(org_dir.join("binary.toml"), [0xff, 0xfe])?;
        std::fs::write(org_dir.join(".tmp-abandoned"), "benchmark_type = \"eval\"")?;

        let catalog = store.load_org_catalog(&acme).await?;
        assert_eq!(catalog.len(), 1);
        assert!(catalog.contains_key(entry.benchmark_id()));
        let reads = [
            store
                .get_org_benchmark(&acme, &BenchmarkId::try_new("broken")?)
                .await,
            store
                .get_org_benchmark(&acme, &BenchmarkId::try_new("binary")?)
                .await,
        ];
        assert!(
            reads.iter().all(|read| read
                .as_ref()
                .is_err_and(|e| e.is::<UnreadableObjectError>())),
            "{reads:?}"
        );
        Ok(())
    }

    fn sample_submission(job_id: &str, client_id: &str) -> serde_json::Value {
        serde_json::json!({
            "job_id": job_id,
            "benchmark_id": "prefill_throughput_256",
            "benchmark_type": "prefill_throughput",
            "client_id": client_id,
            "submitted_at": "2026-03-10T12:01:00Z",
            "prefill_time_ms": 34.7
        })
    }

    #[tokio::test]
    async fn test_localfs_submission_store_write_list_find_and_mark_processed() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        let job_id = JobId::new_unchecked("job1");
        let body = sample_submission(job_id.as_str(), "ev1_client1");

        store.write_incoming(&job_id, &body).await?;

        let incoming = store.list_incoming(TEST_LIST_LIMIT).await?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0], job_id);

        let record = store
            .get_submission(&job_id)
            .await?
            .context("expected submission")?;
        assert_eq!(record.state, JobState::Incoming);
        assert_eq!(record.body["job_id"], "job1");

        let found = store.find_job(&job_id).await?.context("expected job1")?;
        assert_eq!(found.state, JobState::Incoming);
        assert_eq!(found.body["job_id"], "job1");

        store.mark_processed(&job_id).await?;

        assert!(store.list_incoming(TEST_LIST_LIMIT).await?.is_empty());
        let processed = store
            .get_submission(&job_id)
            .await?
            .context("expected processed submission")?;
        assert_eq!(processed.state, JobState::Processed);
        assert_eq!(processed.body, body);
        assert!(dir.path().join("processed/job1.json.gz").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_score_queue_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let job_id = JobId::new_unchecked("job1");
        let body = sample_submission(job_id.as_str(), "ev1_client1");

        store.enqueue(ScoreQueueStage::ToDo, &job_id, &body).await?;
        assert!(
            dir.path()
                .join("submissions/score-queue/to_do/job1.json")
                .exists()
        );
        assert_eq!(
            store
                .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
                .await?,
            vec![job_id.clone()]
        );
        assert!(
            store
                .list_queue(ScoreQueueStage::ToFinalize, TEST_LIST_LIMIT)
                .await?
                .is_empty()
        );

        let read = store
            .read_queue(ScoreQueueStage::ToDo, &job_id)
            .await?
            .context("expected queued payload")?;
        assert_eq!(read, body);

        store.dequeue(ScoreQueueStage::ToDo, &job_id).await?;
        assert!(
            store
                .list_queue(ScoreQueueStage::ToDo, TEST_LIST_LIMIT)
                .await?
                .is_empty()
        );
        // dequeue is idempotent.
        store.dequeue(ScoreQueueStage::ToDo, &job_id).await?;
        assert!(
            store
                .read_queue(ScoreQueueStage::ToDo, &job_id)
                .await?
                .is_none()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_find_job_sees_score_queue_stages() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().join("submissions"));
        let job_id = JobId::new_unchecked("job1");
        let body = sample_submission(job_id.as_str(), "ev1_client1");

        store.enqueue(ScoreQueueStage::ToDo, &job_id, &body).await?;
        let found = store.find_job(&job_id).await?.context("expected to_do")?;
        assert_eq!(found.state, JobState::Scoring);
        assert_eq!(found.body, body);

        // find_job unwraps the to_finalize wrapper back to the bare body.
        store.dequeue(ScoreQueueStage::ToDo, &job_id).await?;
        let payload = serde_json::json!({ "submission": body, "score": { "accuracy": 0.9 } });
        store
            .enqueue(ScoreQueueStage::ToFinalize, &job_id, &payload)
            .await?;
        let found = store
            .find_job(&job_id)
            .await?
            .context("expected to_finalize")?;
        assert_eq!(found.state, JobState::Scoring);
        assert_eq!(found.body, body);

        store.dequeue(ScoreQueueStage::ToFinalize, &job_id).await?;
        assert!(store.find_job(&job_id).await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_unverified_write_list_delete_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        let client = ClientId::try_new("ev1_pending")?;
        let job_id = JobId::new_unchecked("jobA");
        let body = sample_submission(job_id.as_str(), client.as_str());

        store.write_unverified(&client, &job_id, &body).await?;
        // Partitioned by client_id, not flat.
        assert!(dir.path().join("unverified/ev1_pending/jobA.json").exists());

        let held = store.list_unverified_client(&client).await?;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, job_id);
        assert_eq!(held[0].1["client_id"], "ev1_pending");

        // Held submissions never surface through the scoring/lookup paths.
        assert!(store.list_incoming(TEST_LIST_LIMIT).await?.is_empty());
        assert!(store.find_job(&job_id).await?.is_none());

        store.delete_unverified(&client, &job_id).await?;
        assert!(store.list_unverified_client(&client).await?.is_empty());
        // Deleting a missing object is idempotent.
        store.delete_unverified(&client, &job_id).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_unverified_prune_by_age() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        let client = ClientId::try_new("ev1_pending")?;
        store
            .write_unverified(
                &client,
                &JobId::new_unchecked("j1"),
                &sample_submission("j1", "ev1_pending"),
            )
            .await?;
        store
            .write_unverified(
                &client,
                &JobId::new_unchecked("j2"),
                &sample_submission("j2", "ev1_pending"),
            )
            .await?;

        let summary = store
            .prune_unverified(std::time::Duration::from_secs(3600), false)
            .await?;
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.kept, 2);

        let dry = store
            .prune_unverified(std::time::Duration::ZERO, true)
            .await?;
        assert_eq!(dry.deleted, 2);
        assert_eq!(store.list_unverified_client(&client).await?.len(), 2);

        let live = store
            .prune_unverified(std::time::Duration::ZERO, false)
            .await?;
        assert_eq!(live.deleted, 2);
        assert!(store.list_unverified_client(&client).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_submission_store_get_nonexistent_returns_none() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        assert!(
            store
                .get_submission(&JobId::new_unchecked("nonexistent"))
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_submission_store_find_nonexistent_returns_none() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        assert!(
            store
                .find_job(&JobId::new_unchecked("nonexistent"))
                .await?
                .is_none()
        );
        Ok(())
    }

    /// Re-score tests need the newer copy to carry a later `scored_at`.
    fn make_metric_row_at(
        result_id: &str,
        metric: &str,
        value: f32,
        value_stddev: Option<f32>,
        unit: &str,
        scored_at: i64,
    ) -> anyhow::Result<MetricRow> {
        let mut r = make_metric_row(result_id, metric, value, value_stddev, unit)?;
        r.scored_at = scored_at;
        Ok(r)
    }

    fn make_metric_row(
        result_id: &str,
        metric: &str,
        value: f32,
        value_stddev: Option<f32>,
        unit: &str,
    ) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: result_id.to_string(),
            benchmark_id: BenchmarkId::try_new("prefill_throughput_256")?,
            metric: metric.to_string(),
            client_id: ClientId::try_new("ev1_client1")?,
            runtime_name: Some("llama.cpp".to_string()),
            runtime_version: Some("b5000".to_string()),
            value: Some(value),
            value_stddev,
            unit: unit.to_string(),
            submitted_at: 1_741_608_060_000_000,
            scored_at: 1_741_608_120_000_000,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn test_localfs_warehouse_store_append_and_read_job_metrics() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let rows = vec![
            make_metric_row("job1_0", "ttft", 34.7, Some(1.2), "ms")?,
            make_metric_row("job1_1", "prefill_throughput", 7377.5, None, "tokens/sec")?,
        ];

        store
            .write_partition_metrics(
                &BenchmarkId::try_new("prefill_throughput_256")?,
                &ClientId::try_new("ev1_client1")?,
                "2025-03-10",
                &rows,
            )
            .await?;

        let path = dir
            .path()
            .join("warehouse/benchmark_id=prefill_throughput_256/client_id=ev1_client1/day=2025-03-10/part-0001.parquet");
        assert!(path.exists());

        let job_metrics = store
            .read_job_metrics(
                &BenchmarkId::try_new("prefill_throughput_256")?,
                &ClientId::try_new("ev1_client1")?,
                &JobId::new_unchecked("job1"),
            )
            .await?
            .context("expected job metrics")?;
        assert_eq!(job_metrics.metrics.len(), 2);
        assert_eq!(job_metrics.metrics[0].metric, "ttft");
        assert_eq!(job_metrics.metrics[0].value_stddev, Some(1.2));
        assert_eq!(job_metrics.metrics[1].value_stddev, None);
        assert_eq!(
            job_metrics.scored_at.to_rfc3339(),
            "2025-03-10T12:02:00+00:00"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_read_keeps_latest_on_rescore() -> anyhow::Result<()> {
        // Writes append without dedup, so a re-score leaves two copies.
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let bid = BenchmarkId::try_new("prefill_throughput_256")?;
        let cid = ClientId::try_new("ev1_client1")?;
        let (t1, t2) = (1_000_000_000_i64, 2_000_000_000_i64);

        store
            .write_partition_metrics(
                &bid,
                &cid,
                "2025-03-10",
                &[
                    make_metric_row_at("job1_0", "ttft", 34.7, Some(1.2), "ms", t1)?,
                    make_metric_row_at(
                        "job1_1",
                        "prefill_throughput",
                        7377.5,
                        None,
                        "tokens/sec",
                        t1,
                    )?,
                ],
            )
            .await?;
        store
            .write_partition_metrics(
                &bid,
                &cid,
                "2025-03-10",
                &[
                    make_metric_row_at("job1_0", "ttft", 40.0, Some(2.5), "ms", t2)?,
                    make_metric_row_at(
                        "job1_1",
                        "prefill_throughput",
                        8000.0,
                        None,
                        "tokens/sec",
                        t2,
                    )?,
                ],
            )
            .await?;

        // One set, latest values — not four rows.
        let job_metrics = store
            .read_job_metrics(&bid, &cid, &JobId::new_unchecked("job1"))
            .await?
            .context("expected job metrics")?;
        assert_eq!(job_metrics.metrics.len(), 2);
        let ttft = job_metrics
            .metrics
            .iter()
            .find(|m| m.metric == "ttft")
            .context("expected ttft")?;
        assert_eq!(ttft.value, 40.0);
        assert_eq!(ttft.value_stddev, Some(2.5));
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_read_rescore_drops_stale_metrics() -> anyhow::Result<()> {
        // The whole older run is dropped by `scored_at`, not row by row.
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let bid = BenchmarkId::try_new("prefill_throughput_256")?;
        let cid = ClientId::try_new("ev1_client1")?;
        let (t1, t2) = (1_000_000_000_i64, 2_000_000_000_i64);

        store
            .write_partition_metrics(
                &bid,
                &cid,
                "2025-03-10",
                &[
                    make_metric_row_at("job1_0", "ttft", 1.0, None, "ms", t1)?,
                    make_metric_row_at("job1_1", "decode", 2.0, None, "ms", t1)?,
                ],
            )
            .await?;
        store
            .write_partition_metrics(
                &bid,
                &cid,
                "2025-03-10",
                &[make_metric_row_at("job1_0", "ttft", 9.0, None, "ms", t2)?],
            )
            .await?;

        let m = store
            .read_job_metrics(&bid, &cid, &JobId::new_unchecked("job1"))
            .await?
            .context("expected job metrics")?;
        assert_eq!(
            m.metrics.len(),
            1,
            "stale `decode` from copy-1 must be dropped"
        );
        assert_eq!(m.metrics[0].metric, "ttft");
        assert_eq!(m.metrics[0].value, 9.0);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_read_latest_with_interleaved_job() -> anyhow::Result<()> {
        // A's copy-1, then B, then A's copy-2 (re-score) — B's rows sit between
        // the two copies of A. A must still resolve to copy-2 and B is intact.
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let bid = BenchmarkId::try_new("prefill_throughput_256")?;
        let cid = ClientId::try_new("ev1_client1")?;
        let day = "2025-03-10";
        let (t1, t2) = (1_000_000_000_i64, 2_000_000_000_i64);

        store
            .write_partition_metrics(
                &bid,
                &cid,
                day,
                &[make_metric_row_at("A_0", "ttft", 1.0, None, "ms", t1)?],
            )
            .await?;
        store
            .write_partition_metrics(
                &bid,
                &cid,
                day,
                &[make_metric_row_at("B_0", "ttft", 2.0, None, "ms", t1)?],
            )
            .await?;
        store
            .write_partition_metrics(
                &bid,
                &cid,
                day,
                &[make_metric_row_at("A_0", "ttft", 9.0, None, "ms", t2)?],
            )
            .await?;

        let a = store
            .read_job_metrics(&bid, &cid, &JobId::new_unchecked("A"))
            .await?
            .context("expected A")?;
        assert_eq!(a.metrics.len(), 1);
        assert_eq!(a.metrics[0].value, 9.0, "A resolves to its re-scored copy");

        let b = store
            .read_job_metrics(&bid, &cid, &JobId::new_unchecked("B"))
            .await?
            .context("expected B")?;
        assert_eq!(b.metrics.len(), 1);
        assert_eq!(b.metrics[0].value, 2.0, "B is untouched by A's re-score");
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_store_rolls_part_files_at_cap() -> anyhow::Result<()> {
        // max_rows_per_part = 2: the tail part fills to 2 rows, then rolls.
        let dir = tempfile::tempdir()?;
        let warehouse_dir = dir.path().join("warehouse");
        let store =
            LocalFsWarehouseStore::new(warehouse_dir.clone(), 36_500, 2, WriterOpts::default());
        let bid = BenchmarkId::try_new("prefill_throughput_256")?;
        let cid = ClientId::try_new("ev1_client1")?;
        let partition =
            warehouse::warehouse_day_partition_dir(&warehouse_dir, &bid, &cid, "2025-03-10");

        let part_count = || -> anyhow::Result<usize> {
            Ok(std::fs::read_dir(&partition)?
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
                .count())
        };

        // First write: 3 rows → part-0001 (2 rows) + part-0002 (1 row).
        store
            .write_partition_metrics(
                &bid,
                &cid,
                "2025-03-10",
                &[
                    make_metric_row("j1_0", "ttft", 1.0, None, "ms")?,
                    make_metric_row("j1_1", "ttft", 2.0, None, "ms")?,
                    make_metric_row("j1_2", "ttft", 3.0, None, "ms")?,
                ],
            )
            .await?;
        assert_eq!(part_count()?, 2);

        // Second write: 2 rows → tops up part-0002 to 2, then part-0003 (1 row).
        store
            .write_partition_metrics(
                &bid,
                &cid,
                "2025-03-10",
                &[
                    make_metric_row("j2_0", "ttft", 4.0, None, "ms")?,
                    make_metric_row("j2_1", "ttft", 5.0, None, "ms")?,
                ],
            )
            .await?;
        assert_eq!(part_count()?, 3);
        // All rows are present across the roll (append, no loss): j1 has 3
        // rows (spanning parts), j2 has 2.
        assert_eq!(
            store
                .read_job_metrics(&bid, &cid, &JobId::new_unchecked("j1"))
                .await?
                .context("expected j1")?
                .metrics
                .len(),
            3
        );
        assert_eq!(
            store
                .read_job_metrics(&bid, &cid, &JobId::new_unchecked("j2"))
                .await?
                .context("expected j2")?
                .metrics
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_store_read_nonexistent_returns_none() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let result = store
            .read_job_metrics(
                &BenchmarkId::try_new("bench1")?,
                &ClientId::try_new("client1")?,
                &JobId::new_unchecked("nonexistent"),
            )
            .await?;
        assert!(result.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_store_read_finds_job() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let rows = vec![make_metric_row("job1_0", "ttft", 34.7, None, "ms")?];

        store
            .write_partition_metrics(
                &BenchmarkId::try_new("prefill_throughput_256")?,
                &ClientId::try_new("ev1_client1")?,
                "2025-03-10",
                &rows,
            )
            .await?;

        let result = store
            .read_job_metrics(
                &BenchmarkId::try_new("prefill_throughput_256")?,
                &ClientId::try_new("ev1_client1")?,
                &JobId::new_unchecked("job1"),
            )
            .await?
            .context("expected job metrics via scan")?;
        assert_eq!(result.metrics.len(), 1);
        assert_eq!(result.metrics[0].metric, "ttft");

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_read_window_caps_old_partitions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse_dir = dir.path().join("warehouse");
        // 14-day window.
        let store =
            LocalFsWarehouseStore::new(warehouse_dir.clone(), 14, 10_000, WriterOpts::default());
        let bid = BenchmarkId::try_new("prefill_throughput_256")?;
        let cid = ClientId::try_new("ev1_client1")?;

        let today_key = warehouse::day_key_from_timestamp(chrono::Utc::now().timestamp_micros())?;
        store
            .write_partition_metrics(
                &bid,
                &cid,
                &today_key,
                &[make_metric_row("new1_0", "ttft", 1.0, None, "ms")?],
            )
            .await?;
        assert!(
            store
                .read_job_metrics(&bid, &cid, &JobId::new_unchecked("new1"))
                .await?
                .is_some()
        );

        // A `month=` partition outside the window is not returned: there is no
        // full-scan fallback.
        let legacy =
            warehouse::warehouse_month_partition_dir(&warehouse_dir, &bid, &cid, "2020-01");
        warehouse::write_partition(
            WriterOpts::default(),
            &legacy,
            &[make_metric_row("old1_0", "ttft", 2.0, None, "ms")?],
            10_000,
        )?;
        assert!(
            store
                .read_job_metrics(&bid, &cid, &JobId::new_unchecked("old1"))
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_store_write_empty_rows_errors() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsWarehouseStore::new(
            dir.path().join("warehouse"),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        let result = store
            .write_partition_metrics(
                &BenchmarkId::try_new("bench1")?,
                &ClientId::try_new("client1")?,
                "2025-03-10",
                &[],
            )
            .await;
        assert!(result.is_err());
        Ok(())
    }

    /// A stranded `.tmp-*.parquet` from an older binary must not fail the walk.
    #[tokio::test]
    async fn test_localfs_warehouse_walk_skips_stranded_tmp_parquet() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let warehouse_dir = dir.path().join("warehouse");
        let store = LocalFsWarehouseStore::new(
            warehouse_dir.clone(),
            36_500,
            10_000,
            WriterOpts::default(),
        );
        store
            .write_partition_metrics(
                &BenchmarkId::try_new("bench1")?,
                &ClientId::try_new("client1")?,
                "2025-03-10",
                &[make_metric_row("job1_0", "ttft", 34.7, None, "ms")?],
            )
            .await?;

        // No `results/` segment: the constructor takes the results dir itself.
        let partition = warehouse_dir
            .join("benchmark_id=bench1")
            .join("client_id=client1")
            .join("day=2025-03-10");
        std::fs::write(partition.join(".tmp-deadbeef.parquet"), b"not parquet")?;

        // `true` sends the walk down its rewrite path, so this covers both the
        // reader's skip and the writer's `.tmp` staging.
        let mut seen = 0usize;
        store
            .for_each_metric_row(&mut |_row| {
                seen += 1;
                true
            })
            .await?;
        assert_eq!(
            seen, 1,
            "the real part file's row, and nothing from the tmp"
        );

        let staging: Vec<String> = std::fs::read_dir(&partition)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect();
        assert_eq!(
            staging,
            vec![".tmp-deadbeef.parquet".to_string()],
            "the rewrite must leave the planted orphan and no staging file of its own"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_catalog_store_load() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let benchmarks_dir = dir.path().join("benchmarks");
        std::fs::create_dir_all(&benchmarks_dir)?;

        std::fs::write(
            benchmarks_dir.join("prefill_throughput_256.toml"),
            r#"benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256"#,
        )?;

        let store = LocalFsCatalogStore::new(dir.path());
        let catalog = store.load_catalog().await?;
        assert!(catalog.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));

        Ok(())
    }

    /// An unparsable marker still withdraws the fallback: existence alone is
    /// the auth decision. The operator listing skips it instead, by design.
    #[tokio::test]
    async fn test_localfs_an_unreadable_marker_still_reads_as_migrated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());
        let client_id = ClientId::try_new("ev1_unreadable")?;

        // The marker tree is a sibling of `clients/`, not a child of it.
        let markers = dir.path().join("signature-migration");
        std::fs::create_dir_all(&markers)?;
        std::fs::write(markers.join(format!("{client_id}.json")), b"")?;

        assert!(store.has_signature_migration(&client_id).await?);
        assert!(store.list_signature_migrations().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_eval_sample_result_store_write_and_read() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsEvalSampleResultStore::new(
            dir.path().join("eval_sample_results"),
            WriterOpts::default(),
        );

        let rows = vec![
            EvalSampleResult {
                id: "s1".to_string(),
                messages: r#"[{"role":"user","content":"What is 2+2?"}]"#.to_string(),
                completion: "4".to_string(),
                is_correct: true,
                failed: false,
                failed_reason: None,
                stop_reason: None,
                stop_reason_source: None,
                stop_detail: None,
                completion_tokens: None,
                measures: None,
                chat_template_kwargs: None,
            },
            EvalSampleResult {
                id: "s2".to_string(),
                messages: r#"[{"role":"user","content":"Capital of France?"}]"#.to_string(),
                completion: "London".to_string(),
                is_correct: false,
                failed: false,
                failed_reason: None,
                stop_reason: None,
                stop_reason_source: None,
                stop_detail: None,
                completion_tokens: None,
                measures: None,
                chat_template_kwargs: None,
            },
        ];

        store.write(&JobId::new_unchecked("job1"), &rows).await?;

        let result = store
            .read(&JobId::new_unchecked("job1"))
            .await?
            .context("expected eval sample results")?;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "s1");
        assert!(result[0].is_correct);
        assert_eq!(result[1].id, "s2");
        assert!(!result[1].is_correct);

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_eval_sample_result_store_read_nonexistent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsEvalSampleResultStore::new(
            dir.path().join("eval_sample_results"),
            WriterOpts::default(),
        );

        let result = store.read(&JobId::new_unchecked("nonexistent")).await?;
        assert!(result.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_eval_sample_result_store_no_tmp_after_write() -> anyhow::Result<()> {
        // A successful write leaves exactly the final `.parquet`, no tmp name.
        let dir = tempfile::tempdir()?;
        let esr_dir = dir.path().join("eval_sample_results");
        let store = LocalFsEvalSampleResultStore::new(esr_dir.clone(), WriterOpts::default());

        let rows = vec![EvalSampleResult {
            id: "s1".to_string(),
            messages: r#"[{"role":"user","content":"Q"}]"#.to_string(),
            completion: "A".to_string(),
            is_correct: true,
            failed: false,
            failed_reason: None,
            stop_reason: None,
            stop_reason_source: None,
            stop_detail: None,
            completion_tokens: None,
            measures: None,
            chat_template_kwargs: None,
        }];
        store.write(&JobId::new_unchecked("job1"), &rows).await?;

        let entries: Vec<_> = std::fs::read_dir(&esr_dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["job1.parquet".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_eval_sample_result_list_job_ids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let esr_dir = dir.path().join("eval_sample_results");
        let store = LocalFsEvalSampleResultStore::new(esr_dir.clone(), WriterOpts::default());

        // Absent dir lists nothing.
        assert!(store.list_job_ids().await?.is_empty());

        let rows = vec![EvalSampleResult {
            id: "s1".to_string(),
            messages: "[]".to_string(),
            completion: "A".to_string(),
            is_correct: true,
            failed: false,
            failed_reason: None,
            stop_reason: None,
            stop_reason_source: None,
            stop_detail: None,
            completion_tokens: None,
            measures: None,
            chat_template_kwargs: None,
        }];
        store.write(&JobId::new_unchecked("job-a"), &rows).await?;
        store.write(&JobId::new_unchecked("job-b"), &rows).await?;

        // Junk to skip: a tmp file, a non-parquet file, a bad-stem parquet.
        std::fs::write(esr_dir.join(".tmp-deadbeef.parquet"), b"x")?;
        std::fs::write(esr_dir.join("notes.txt"), b"x")?;
        std::fs::write(esr_dir.join("invalid_name.parquet"), b"x")?;

        let mut ids: Vec<String> = store
            .list_job_ids()
            .await?
            .iter()
            .map(|j| j.as_str().to_string())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["job-a".to_string(), "job-b".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_eval_sample_result_store_overwrite_on_retry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsEvalSampleResultStore::new(
            dir.path().join("eval_sample_results"),
            WriterOpts::default(),
        );

        let rows_v1 = vec![EvalSampleResult {
            id: "s1".to_string(),
            messages: r#"[{"role":"user","content":"Q1"}]"#.to_string(),
            completion: "A".to_string(),
            is_correct: false,
            failed: false,
            failed_reason: None,
            stop_reason: None,
            stop_reason_source: None,
            stop_detail: None,
            completion_tokens: None,
            measures: None,
            chat_template_kwargs: None,
        }];

        store.write(&JobId::new_unchecked("job1"), &rows_v1).await?;

        let rows_v2 = vec![EvalSampleResult {
            id: "s1".to_string(),
            messages: r#"[{"role":"user","content":"Q1"}]"#.to_string(),
            completion: "B".to_string(),
            is_correct: true,
            failed: false,
            failed_reason: None,
            stop_reason: None,
            stop_reason_source: None,
            stop_detail: None,
            completion_tokens: None,
            measures: None,
            chat_template_kwargs: None,
        }];

        store.write(&JobId::new_unchecked("job1"), &rows_v2).await?;

        let result = store
            .read(&JobId::new_unchecked("job1"))
            .await?
            .context("expected eval sample results")?;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].completion, "B");
        assert!(result[0].is_correct);

        Ok(())
    }

    // ── TodoStore ───────────────────────────────────────────────────────────

    use std::time::Duration;

    use chrono::DateTime;

    use pipette_core::todo_filename::{avail_filename, leased_key};
    use pipette_plan_types::{CampaignId, ExpiresAt};
    use pipette_store::{
        CampaignBacklog, ClaimResult, RecycleResult, RenewLeaseResult, SuspensionRecord, TodoStore,
    };

    /// Creates the `{client_id}/` partition dir first, as the layout requires.
    fn plant_leased(
        leased_dir: &std::path::Path,
        job: &JobId,
        expires_at: ExpiresAt,
        client: &ClientId,
        expiry: DateTime<Utc>,
        body: &[u8],
    ) -> anyhow::Result<()> {
        let path = leased_dir.join(leased_key(job, expires_at, client, expiry));
        std::fs::create_dir_all(
            path.parent()
                .context("the fixture path has a parent directoryath")?,
        )?;
        std::fs::write(path, body)?;
        Ok(())
    }

    /// A store over a fresh tempdir with the `todo/` layout that
    /// `build_local_fs_stores` creates, plus the `todo/` root for fixtures.
    fn todo_store() -> anyhow::Result<(LocalFsTodoStore, tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir()?;
        let todo_dir = dir.path().join("todo");
        [
            "avail",
            "leased",
            "denied",
            "eligible/clients",
            "pending-reindex",
            "pending-reindex-jobs",
            "lost",
            "tmp",
            "suspended",
            "liveness",
        ]
        .into_iter()
        .try_for_each(|sub| std::fs::create_dir_all(todo_dir.join(sub)))?;
        let store = LocalFsTodoStore::new(todo_dir.clone());
        Ok((store, dir, todo_dir))
    }

    fn dt(s: &str) -> anyhow::Result<DateTime<Utc>> {
        Ok(s.parse::<DateTime<Utc>>()?)
    }

    /// Write a body straight into `avail/{job_id}/{expires_at}.json`.
    fn seed_avail(
        todo_dir: &Path,
        job_id: &JobId,
        expires_at: ExpiresAt,
        body: &[u8],
    ) -> anyhow::Result<()> {
        let path = todo_dir
            .join("avail")
            .join(avail_filename(job_id, expires_at));
        std::fs::create_dir_all(path.parent().context("an avail key has a job directory")?)?;
        Ok(std::fs::write(path, body)?)
    }

    #[tokio::test]
    async fn test_localfs_write_tmp_then_promote_avail() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");
        let exp = ExpiresAt::At(dt("2026-08-01T00:00:00Z")?);
        let body = serde_json::json!({ "job_id": "job1", "expires_at": "20260801T000000Z" });

        store.write_tmp(&job, &body).await?;
        // Staged in tmp/, not yet claimable — nothing in avail/.
        assert!(todo_dir.join("tmp").join("job1.json").exists());
        assert!(store.list_avail().await?.is_empty());

        store.promote_avail(&job, exp).await?;
        // A partial (tmp-only) write never appears in avail/.
        assert!(!todo_dir.join("tmp").join("job1.json").exists());
        assert_eq!(store.list_avail().await?, vec![avail_filename(&job, exp)]);
        let got = store
            .get_avail(&job, exp)
            .await?
            .context("expected avail body")?;
        assert_eq!(got["job_id"], "job1");
        Ok(())
    }

    /// The body keeps its ingested `expires_at`, so key and body disagree from
    /// here on and the key wins.
    #[tokio::test]
    async fn test_localfs_redate_avail_moves_the_entry_and_keeps_the_body() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");
        let from = ExpiresAt::At(dt("2026-08-01T00:00:00Z")?);
        let to = ExpiresAt::At(dt("2026-09-01T00:00:00Z")?);
        let body = serde_json::json!({ "job_id": "job1", "expires_at": "20260801T000000Z" });
        store.write_tmp(&job, &body).await?;
        store.promote_avail(&job, from).await?;

        assert_eq!(
            store.redate_avail(&job, from, to).await?,
            RedateResult::Redated
        );
        assert_eq!(store.list_avail().await?, vec![avail_filename(&job, to)]);
        assert_eq!(
            store.avail_key_for_job(&job).await?,
            Some(avail_filename(&job, to))
        );
        let got = store
            .get_avail(&job, to)
            .await?
            .context("expected avail body at the new deadline")?;
        assert_eq!(got, body);
        Ok(())
    }

    /// An absent source is `Gone`, as `claim_job` reads a lost race.
    #[tokio::test]
    async fn test_localfs_redate_avail_reports_an_absent_source_gone() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");
        let from = ExpiresAt::At(dt("2026-08-01T00:00:00Z")?);
        assert_eq!(
            store.redate_avail(&job, from, ExpiresAt::Never).await?,
            RedateResult::Gone
        );
        assert!(store.avail_key_for_job(&job).await?.is_none());
        Ok(())
    }

    /// Which job-scoped read a case exercises; the S3 store holds the same table.
    enum JobScoped {
        Body,
        Key,
        Delete,
    }

    /// Each job-scoped read sees its own job's entry and no other's, and the key
    /// it reports is the form the filename parser reads.
    #[rstest]
    #[case::body(JobScoped::Body)]
    #[case::key(JobScoped::Key)]
    #[case::delete(JobScoped::Delete)]
    #[tokio::test]
    async fn test_localfs_a_job_scoped_read_sees_only_its_own_job(
        #[case] method: JobScoped,
    ) -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let mine = JobId::new_unchecked("job1");
        let other = JobId::new_unchecked("job2");
        let exp = ExpiresAt::At(dt("2026-08-01T00:00:00Z")?);
        seed_avail(&todo_dir, &mine, exp, br#"{"job_id":"job1"}"#)?;
        seed_avail(&todo_dir, &other, ExpiresAt::Never, br#"{"job_id":"job2"}"#)?;

        match method {
            JobScoped::Body => {
                let body = store
                    .get_avail_by_job(&mine)
                    .await?
                    .context("expected this job's body")?;
                assert_eq!(body["job_id"], "job1");
            }
            JobScoped::Key => {
                let name = store
                    .avail_key_for_job(&mine)
                    .await?
                    .context("expected this job's avail key")?;
                assert_eq!(name, avail_filename(&mine, exp));
                assert_eq!(
                    pipette_core::todo_filename::parse_avail_filename(&name)?,
                    (mine.clone(), exp)
                );
            }
            JobScoped::Delete => store.delete_avail_by_job(&mine).await?,
        }

        // The other job is untouched whichever read ran.
        assert_eq!(
            store.get_avail_by_job(&other).await?,
            Some(serde_json::json!({"job_id": "job2"}))
        );
        Ok(())
    }

    /// The content-key index is held to the shared contract, so the digest-based
    /// version check answers the same questions S3's `If-Match` does.
    #[tokio::test]
    async fn test_localfs_content_key_index_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_content_key_roundtrip(&LocalFsPlanStore::new(dir.path()))
            .await
    }

    #[tokio::test]
    async fn test_localfs_plan_filter_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_plan_filter_contract(&LocalFsPlanStore::new(dir.path())).await
    }

    #[tokio::test]
    async fn test_localfs_plan_reach_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_plan_reach_contract(&LocalFsPlanStore::new(dir.path())).await
    }

    /// The plan listing is held to the shared pagination contract, so the
    /// filesystem sort answers the same questions S3's `start-after` does.
    #[tokio::test]
    async fn test_localfs_plan_listing_meets_the_shared_pagination_contract() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let plans_dir = dir.path().join("plans");
        pipette_store_testkit::assert_plan_listing_pagination(
            &LocalFsPlanStore::new(dir.path()),
            |leaf, bytes| {
                let path = plans_dir.join(leaf);
                async move {
                    std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
                    std::fs::write(&path, bytes)?;
                    Ok(())
                }
            },
        )
        .await
    }

    #[tokio::test]
    async fn test_localfs_get_plans_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let plans_dir = dir.path().join("plans");
        pipette_store_testkit::assert_get_plans_contract(
            &LocalFsPlanStore::new(dir.path()),
            |leaf, bytes| {
                let path = plans_dir.join(leaf);
                async move {
                    std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
                    std::fs::write(&path, bytes)?;
                    Ok(())
                }
            },
        )
        .await
    }

    /// The exclusive incoming write is held to the shared contract, so
    /// `hard_link` refuses a taken key exactly where S3's `If-None-Match` does.
    #[tokio::test]
    async fn test_localfs_exclusive_incoming_write_meets_the_shared_contract() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_exclusive_incoming_write(&LocalFsSubmissionStore::new(
            dir.path().to_path_buf(),
        ))
        .await
    }

    #[tokio::test]
    async fn test_localfs_has_record_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_has_record_contract(&LocalFsSubmissionStore::new(
            dir.path().to_path_buf(),
        ))
        .await
    }

    #[tokio::test]
    async fn test_localfs_find_outcomes_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_find_outcomes_contract(&LocalFsSubmissionStore::new(
            dir.path().to_path_buf(),
        ))
        .await
    }

    /// `has_record` errors rather than answer `false` ("no result"); a file in
    /// place of `to_do/` makes the check fail with `ENOTDIR`.
    #[tokio::test]
    async fn test_localfs_has_record_returns_an_io_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsSubmissionStore::new(dir.path().to_path_buf());
        let to_do = store.stage_dir(pipette_store::ScoreQueueStage::ToDo);
        let parent = to_do
            .parent()
            .ok_or_else(|| anyhow::anyhow!("stage dir has no parent dir"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(&to_do, b"not a directory")?;

        let found = store.has_record(&JobId::new_unchecked("job-1")).await;
        assert!(found.is_err(), "{found:?}");
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_runtime_output_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_runtime_output_roundtrip(&LocalFsSubmissionStore::new(
            dir.path().to_path_buf(),
        ))
        .await
    }

    #[tokio::test]
    async fn test_localfs_submission_listings_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_submission_listings_page(&LocalFsSubmissionStore::new(
            dir.path().to_path_buf(),
        ))
        .await
    }

    /// The staging name goes on both paths; a leak on the winning one would
    /// cost one file per claim.
    #[tokio::test]
    async fn test_localfs_content_key_claim_leaves_no_staging_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        let digest = "d".repeat(64);
        let record = ContentKeyRecord {
            job_id: JobId::from_uuid(uuid::Uuid::from_u128(1)),
            plan_id: PlanId::from_uuid(uuid::Uuid::from_u128(1)),
            created_at: Utc::now(),
        };

        assert!(matches!(
            store.create_content_key(&digest, &record).await?,
            ContentKeyClaim::Created
        ));
        // The losing claim stages its own bytes, so it has its own file to drop.
        assert!(matches!(
            store.create_content_key(&digest, &record).await?,
            ContentKeyClaim::AlreadyExists(..)
        ));

        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("content-keys").join("v1"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec![digest], "the record is the only file left");
        Ok(())
    }

    /// The staging name goes on the winning put and on the refused one.
    #[tokio::test]
    async fn test_localfs_source_puts_leave_no_staging_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(1));
        let source = PlanSource::new("benchmarks = [\"b\"]\n".to_string())?;
        let body = serde_json::json!({ "job_id": job_id.as_str() });

        for _ in 0..2 {
            store.put_plan_source(&plan_id, &source).await?;
            store.put_job_source(&job_id, &body).await?;
        }

        let names = |subdir: &str| -> anyhow::Result<Vec<String>> {
            Ok(std::fs::read_dir(dir.path().join(subdir))?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect())
        };
        assert_eq!(
            names("plan-sources")?,
            vec![format!("{plan_id}.toml")],
            "the plan source is the only file left"
        );
        assert_eq!(
            names("job-sources")?,
            vec![format!("{job_id}.json")],
            "the job source is the only file left"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_job_plan_markers_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_job_plan_marker_roundtrip(&LocalFsPlanStore::new(dir.path()))
            .await
    }

    #[tokio::test]
    async fn test_localfs_job_sources_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_job_source_roundtrip(&LocalFsPlanStore::new(dir.path())).await
    }

    #[tokio::test]
    async fn test_localfs_job_plan_links_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_job_plan_link_roundtrip(&LocalFsPlanStore::new(dir.path()))
            .await
    }

    #[tokio::test]
    async fn test_localfs_job_plan_links_skip_a_foreign_key() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(1));
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        store.put_job_plan_link(&job_id, &plan_id).await?;
        std::fs::write(store.job_plan_links_dir_for(&job_id).join("stray.tmp"), b"")?;

        assert_eq!(store.list_job_plan_links(&job_id).await?, vec![plan_id]);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_plan_sources_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_plan_source_roundtrip(&LocalFsPlanStore::new(dir.path()))
            .await
    }

    #[tokio::test]
    async fn test_localfs_pending_source_markers_meet_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_pending_source_marker_roundtrip(&LocalFsPlanStore::new(
            dir.path(),
        ))
        .await
    }

    /// A vanished path is absent; a read that failed for any other reason propagates.
    #[test]
    fn test_vanished_is_absent_skips_only_a_missing_path() -> anyhow::Result<()> {
        use std::io::{Error, ErrorKind};

        assert_eq!(vanished_is_absent(Ok::<u8, Error>(7))?, Some(7));
        assert_eq!(
            vanished_is_absent(Err::<u8, _>(Error::new(ErrorKind::NotFound, "gone")))?,
            None
        );
        assert!(
            vanished_is_absent(Err::<u8, _>(Error::new(
                ErrorKind::PermissionDenied,
                "unreadable"
            )))
            .is_err(),
            "an unreadable path is not an absent one"
        );
        Ok(())
    }

    /// A crash between `create_dir_all` and the marker write leaves an empty
    /// job dir; it must not cost the run its listing.
    #[tokio::test]
    async fn test_localfs_all_job_plan_markers_skips_an_empty_job_dir() -> anyhow::Result<()> {
        use pipette_plan_types::PlanId;

        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        let held = JobId::new_unchecked("job-held");
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));

        store
            .write_job_plan_marker(&held, &plan_id, ExpiresAt::Never)
            .await?;
        std::fs::create_dir_all(store.job_plans_dir_for(&JobId::new_unchecked("job-bare")))?;

        let markers = store.list_all_job_plan_markers().await?;
        assert_eq!(
            markers.iter().map(|m| &m.job_id).collect::<Vec<_>>(),
            vec![&held],
            "only the job that has a marker is listed"
        );
        Ok(())
    }

    /// A job that keeps a member keeps its directory.
    #[tokio::test]
    async fn test_localfs_the_last_marker_takes_the_job_dir_with_it() -> anyhow::Result<()> {
        use pipette_plan_types::PlanId;

        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        let job_id = JobId::new_unchecked("job-shared");
        let first = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let second = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        let job_dir = store.job_plans_dir_for(&job_id);

        store
            .write_job_plan_marker(&job_id, &first, ExpiresAt::Never)
            .await?;
        store
            .write_job_plan_marker(&job_id, &second, ExpiresAt::Never)
            .await?;

        store
            .delete_job_plan_marker(&job_id, &first, ExpiresAt::Never)
            .await?;
        assert!(
            job_dir.try_exists()?,
            "a job with a surviving member keeps its directory"
        );
        assert_eq!(store.list_job_plan_markers(&job_id).await?.len(), 1);

        store
            .delete_job_plan_marker(&job_id, &second, ExpiresAt::Never)
            .await?;
        assert!(
            !job_dir.try_exists()?,
            "the last marker takes the directory with it"
        );
        assert!(store.list_job_plan_markers(&job_id).await?.is_empty());
        Ok(())
    }

    /// The write recreates a directory the last marker's delete took, and
    /// retries.
    #[tokio::test]
    async fn test_localfs_marker_write_survives_a_removed_job_dir() -> anyhow::Result<()> {
        use pipette_plan_types::PlanId;

        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        let job_id = JobId::new_unchecked("job-rejoined");
        let leaving = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let joining = PlanId::from_uuid(uuid::Uuid::from_u128(2));

        store
            .write_job_plan_marker(&job_id, &leaving, ExpiresAt::Never)
            .await?;
        store
            .delete_job_plan_marker(&job_id, &leaving, ExpiresAt::Never)
            .await?;

        store
            .write_job_plan_marker(&job_id, &joining, ExpiresAt::Never)
            .await?;
        assert_eq!(
            store.list_job_plan_markers(&job_id).await?,
            vec![(joining, ExpiresAt::Never)],
            "the joiner's membership is recorded in a directory that had gone"
        );
        Ok(())
    }

    /// An I/O failure stays fatal where a parse failure is reported. A directory
    /// named `*.json` makes `read` fail with something other than `NotFound`.
    #[tokio::test]
    async fn test_localfs_a_plan_listing_fails_when_a_manifest_cannot_be_read() -> anyhow::Result<()>
    {
        use pipette_core::plan::sample_manifest;
        use pipette_plan_types::PlanId;
        use pipette_plan_types::PlanStatus;

        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());
        store
            .put_plan(&sample_manifest(
                PlanId::try_new("plan-good")?,
                PlanStatus::Active,
                false,
            )?)
            .await?;
        std::fs::create_dir(dir.path().join("plans").join("plan-unreadable.json"))?;

        assert!(
            store
                .list_plans(None, None, pipette_store_testkit::TEST_LIST_LIMIT)
                .await
                .is_err(),
            "an unreadable object is not a report, because we never saw its contents"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_plan_store_roundtrip_list_delete() -> anyhow::Result<()> {
        use pipette_core::plan::sample_manifest;
        use pipette_plan_types::PlanId;
        use pipette_plan_types::PlanStatus;

        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());

        let active_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let done_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        // Round-trips with a progress_snapshot and without one.
        let active = sample_manifest(active_id.clone(), PlanStatus::Active, true)?;
        let done = sample_manifest(done_id.clone(), PlanStatus::Complete, false)?;
        store.put_plan(&active).await?;
        store.put_plan(&done).await?;

        assert_eq!(store.get_plan(&active_id).await?.as_ref(), Some(&active));
        assert_eq!(store.get_plan(&done_id).await?.as_ref(), Some(&done));

        // `None` returns all; a status filter narrows it.
        let all = pipette_store::drain_plans(&store, None).await?.items;
        assert_eq!(all, vec![active.clone(), done.clone()]);
        assert_eq!(
            pipette_store::drain_plans(&store, Some(PlanStatus::Active))
                .await?
                .items,
            vec![active]
        );
        assert!(
            pipette_store::drain_plans(&store, Some(PlanStatus::Cancelled))
                .await?
                .items
                .is_empty()
        );

        // A never-created plan reads back as None.
        let absent = PlanId::from_uuid(uuid::Uuid::from_u128(99));
        assert!(store.get_plan(&absent).await?.is_none());
        Ok(())
    }

    /// Cancel markers round-trip, are idempotent in both directions, and live in
    /// a keyspace `list_plans` cannot see.
    #[tokio::test]
    async fn test_localfs_cancel_marker_roundtrip() -> anyhow::Result<()> {
        use pipette_core::plan::sample_manifest;
        use pipette_plan_types::PlanId;
        use pipette_plan_types::PlanStatus;

        let dir = tempfile::tempdir()?;
        let store = LocalFsPlanStore::new(dir.path());

        let a = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let b = PlanId::from_uuid(uuid::Uuid::from_u128(2));

        // No marker yet; listing an absent directory is empty, not an error.
        assert!(!store.has_cancel_marker(&a).await?);
        assert!(store.list_cancel_markers().await?.is_empty());

        store.write_cancel_marker(&a).await?;
        assert!(store.has_cancel_marker(&a).await?);
        assert!(!store.has_cancel_marker(&b).await?, "marker is per-plan");
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        // Re-cancelling is a no-op: the marker is a request, not a swap.
        store.write_cancel_marker(&a).await?;
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        // A marker does not show in `list_plans`.
        store
            .put_plan(&sample_manifest(a.clone(), PlanStatus::Active, false)?)
            .await?;
        assert_eq!(
            pipette_store::drain_plans(&store, None).await?.items.len(),
            1
        );

        // Deleting the marker leaves the manifest alone, and is idempotent.
        store.delete_cancel_marker(&a).await?;
        assert!(!store.has_cancel_marker(&a).await?);
        store.delete_cancel_marker(&a).await?; // no-op, no error
        assert!(store.get_plan(&a).await?.is_some(), "manifest survives");
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_cancel_marker_skips_unparseable() -> anyhow::Result<()> {
        use pipette_plan_types::PlanId;

        let dir = tempfile::tempdir()?;
        let cancelled = dir.path().join("cancelled_plans");
        std::fs::create_dir_all(&cancelled)?;
        let store = LocalFsPlanStore::new(dir.path());

        let good = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        store.write_cancel_marker(&good).await?;
        // `.` is outside the PlanId charset.
        std::fs::write(cancelled.join("not.a.plan.id"), b"")?;

        assert_eq!(store.list_cancel_markers().await?, vec![good]);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_pending_source_marker_skips_unparseable() -> anyhow::Result<()> {
        use pipette_plan_types::PlanId;

        let dir = tempfile::tempdir()?;
        let pending = dir.path().join("plan-sources-pending");
        std::fs::create_dir_all(&pending)?;
        let store = LocalFsPlanStore::new(dir.path());

        let good = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        store.write_pending_source_marker(&good).await?;
        std::fs::write(pending.join("not.a.plan.id"), b"")?;

        let listed: Vec<PlanId> = store
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|m| m.plan_id)
            .collect();
        assert_eq!(listed, vec![good]);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_claim_happy_path_then_gone() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");
        let client = ClientId::try_new("client1")?;
        let exp = ExpiresAt::Never;
        let lease = dt("2026-06-15T08:30:00Z")?;

        let body = serde_json::json!({ "job_id": "job1", "expires_at": "never" });
        seed_avail(&todo_dir, &job, exp, &serde_json::to_vec(&body)?)?;

        match store.claim_job(&job, exp, &client, lease).await? {
            ClaimResult::Claimed(got) => assert_eq!(got["job_id"], "job1"),
            ClaimResult::Gone => anyhow::bail!("expected Claimed, got Gone"),
        }

        // The emptied job directory went too, mirroring Express's auto-reclaim.
        assert!(store.list_avail().await?.is_empty());
        assert!(!todo_dir.join("avail").join(job.as_str()).exists());
        assert_eq!(
            store.list_leased().await?,
            vec![leased_key(&job, ExpiresAt::Never, &client, lease)]
        );
        // The targeted per-client lookup finds the same single lease.
        assert_eq!(
            store.list_leased_for_client(&client).await?,
            vec![leased_key(&job, ExpiresAt::Never, &client, lease)]
        );

        // Re-claiming the now-absent avail key loses the race → Gone.
        assert!(matches!(
            store.claim_job(&job, exp, &client, lease).await?,
            ClaimResult::Gone
        ));

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_delete_avail_by_job_removes_every_expiry_for_job() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");
        let other = JobId::new_unchecked("job2");
        let avail = todo_dir.join("avail");

        // Two expiries: teardown knows the job_id but not the expiry on disk.
        for exp in [ExpiresAt::Never, ExpiresAt::At(dt("2026-06-15T08:30:00Z")?)] {
            seed_avail(&todo_dir, &job, exp, b"{}")?;
        }
        // A different job must survive the by-job delete.
        seed_avail(&todo_dir, &other, ExpiresAt::Never, b"{}")?;

        store.delete_avail_by_job(&job).await?;

        assert_eq!(
            store.list_avail().await?,
            vec![avail_filename(&other, ExpiresAt::Never)],
            "only the targeted job's entries (all expiries) are removed"
        );
        assert!(
            !avail.join(job.as_str()).exists(),
            "the emptied job directory goes with its last entry"
        );

        // Idempotent: a second call with nothing matching is a no-op.
        store.delete_avail_by_job(&job).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_renew_lease_branches() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let client = ClientId::try_new("client1")?;
        let old = dt("2026-06-15T08:00:00Z")?;
        let new = dt("2026-06-15T08:05:00Z")?;
        let leased = todo_dir.join("leased");

        // Renewed: the client's own lease is renamed to the new expiry.
        let job_r = JobId::new_unchecked("jobrenew");
        plant_leased(&leased, &job_r, ExpiresAt::Never, &client, old, b"{}")?;
        assert_eq!(
            store.renew_lease(&job_r, &client, new).await?,
            RenewLeaseResult::Renewed(ExpiresAt::Never)
        );
        assert!(
            leased
                .join(leased_key(&job_r, ExpiresAt::Never, &client, new))
                .exists()
        );

        // A dated renewal keeps the job's `expires_at`; the `never` case above
        // would pass a renewal that wrote a constant `never`.
        let job_d = JobId::new_unchecked("jobdated");
        let job_expiry = ExpiresAt::At(dt("2026-09-08T00:00:00Z")?);
        plant_leased(&leased, &job_d, job_expiry, &client, old, b"{}")?;
        assert_eq!(
            store.renew_lease(&job_d, &client, new).await?,
            RenewLeaseResult::Renewed(job_expiry)
        );
        assert!(
            leased
                .join(leased_key(&job_d, job_expiry, &client, new))
                .exists()
        );

        // WrongClient: another client's partition holds the lease.
        let job_w = JobId::new_unchecked("jobwrong");
        let other = ClientId::try_new("client2")?;
        plant_leased(&leased, &job_w, ExpiresAt::Never, &other, old, b"{}")?;
        assert_eq!(
            store.renew_lease(&job_w, &client, new).await?,
            RenewLeaseResult::WrongClient
        );

        // NotFound: no lease exists for the job at all.
        let job_n = JobId::new_unchecked("jobnone");
        assert_eq!(
            store.renew_lease(&job_n, &client, new).await?,
            RenewLeaseResult::NotFound
        );

        Ok(())
    }

    /// A leaf with the job prefix that does not parse is cruft: renewal reports
    /// `NotFound` and leaves it. The scan-past case is pinned by the S3 store's
    /// `renew_skips_unparseable_key`, where the listing order is fixed.
    #[tokio::test]
    async fn test_localfs_todo_renew_lease_skips_unparseable_leaf() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let client = ClientId::try_new("client1")?;
        let old = dt("2026-06-15T08:00:00Z")?;
        let new = dt("2026-06-15T08:05:00Z")?;
        let leased = todo_dir.join("leased");
        let own_dir = leased.join(client.as_str());
        std::fs::create_dir_all(&own_dir)?;

        // Cruft alone: a two-field leaf is the old key shape.
        let job = JobId::new_unchecked("jobcruft");
        let cruft = own_dir.join(format!("{job}.20260615T080000Z.json"));
        std::fs::write(&cruft, b"{}")?;
        assert_eq!(
            store.renew_lease(&job, &client, new).await?,
            RenewLeaseResult::NotFound
        );
        assert!(cruft.exists(), "cruft must be left in place, not renamed");

        // Cruft plus a real lease: the scan renews the one that parses.
        plant_leased(&leased, &job, ExpiresAt::Never, &client, old, b"{}")?;
        assert_eq!(
            store.renew_lease(&job, &client, new).await?,
            RenewLeaseResult::Renewed(ExpiresAt::Never)
        );
        assert!(
            leased
                .join(leased_key(&job, ExpiresAt::Never, &client, new))
                .exists()
        );
        assert!(cruft.exists(), "cruft must survive a successful renewal");
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_list_leased_tolerates_stray_entries() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let leased = todo_dir.join("leased");
        let client = ClientId::try_new("client1")?;
        let job = JobId::new_unchecked("job1");
        let lease = dt("2026-06-15T08:30:00Z")?;
        plant_leased(&leased, &job, ExpiresAt::Never, &client, lease, b"{}")?;

        // A stray file at the partition root and an empty partition are
        // skipped, as when a partition is created or emptied concurrently.
        std::fs::write(leased.join(".DS_Store"), b"junk")?;
        std::fs::create_dir_all(leased.join("emptyclient"))?;

        assert_eq!(
            store.list_leased().await?,
            vec![leased_key(&job, ExpiresAt::Never, &client, lease)]
        );
        Ok(())
    }

    /// The avail name takes `expires_at` from the lease key, never the body
    /// (planted here to disagree). A missing `avail/` directory is recreated
    /// rather than stranding the lease as `Gone`.
    #[rstest]
    #[case::preserves_expires_at(Some("2026-07-01T00:00:00Z"), false)]
    #[case::never(None, false)]
    #[case::recreates_missing_avail_dir(None, true)]
    #[tokio::test]
    async fn test_localfs_todo_recycle_lease_recycles(
        #[case] key_expires_at: Option<&str>,
        #[case] remove_avail_dir: bool,
    ) -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("jobx");
        let client = ClientId::try_new("client1")?;
        let lease = dt("2026-06-20T10:00:00Z")?;

        let expected = match key_expires_at {
            Some(s) => ExpiresAt::At(dt(s)?),
            None => ExpiresAt::Never,
        };
        let body = serde_json::json!({
            "job_id": "jobx",
            "expires_at": "2099-12-31T00:00:00Z",
        });
        plant_leased(
            &todo_dir.join("leased"),
            &job,
            expected,
            &client,
            lease,
            &serde_json::to_vec(&body)?,
        )?;
        if remove_avail_dir {
            std::fs::remove_dir_all(todo_dir.join("avail"))?;
        }

        assert_eq!(
            store.recycle_lease(&job, expected, &client, lease).await?,
            RecycleResult::Recycled
        );

        assert!(store.list_leased().await?.is_empty());
        assert_eq!(
            store.list_avail().await?,
            vec![avail_filename(&job, expected)]
        );
        let recycled = store
            .get_avail(&job, expected)
            .await?
            .context("avail body")?;
        assert_eq!(recycled["job_id"], "jobx");

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_recycle_lease_gone_when_absent() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("jobgone");
        let client = ClientId::try_new("client1")?;
        let lease = dt("2026-06-20T10:00:00Z")?;

        assert_eq!(
            store
                .recycle_lease(&job, ExpiresAt::Never, &client, lease)
                .await?,
            RecycleResult::Gone
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_list_avail_sorts_and_ignores_non_json() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let avail = todo_dir.join("avail");
        // Seeded out of order, so the expectation pins the sort.
        ["b", "c", "a"].into_iter().try_for_each(|id| {
            seed_avail(
                &todo_dir,
                &JobId::new_unchecked(id),
                ExpiresAt::Never,
                b"{}",
            )
        })?;
        // A non-`.json` sibling must be skipped.
        std::fs::write(avail.join("notjson.txt"), b"")?;

        assert_eq!(
            store.list_avail().await?,
            vec![
                "a/never.json".to_string(),
                "b/never.json".to_string(),
                "c/never.json".to_string()
            ]
        );

        Ok(())
    }

    /// An emptied job directory never accumulates in the `avail/` walk.
    #[tokio::test]
    async fn test_localfs_delete_avail_drops_the_emptied_job_directory() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");
        let first = ExpiresAt::Never;
        let second = ExpiresAt::At(dt("2026-06-15T08:30:00Z")?);
        let job_dir = todo_dir.join("avail").join(job.as_str());
        seed_avail(&todo_dir, &job, first, b"{}")?;
        seed_avail(&todo_dir, &job, second, b"{}")?;

        // The directory still holds the second entry, so it stays.
        store.delete_avail(&job, first).await?;
        assert!(
            job_dir.exists(),
            "a directory that still holds an entry stays"
        );

        store.delete_avail(&job, second).await?;
        assert!(
            !job_dir.exists(),
            "the emptied job directory goes with its last entry"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_get_avail_by_job() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");

        // Absent → None (no avail entry for the job).
        assert!(store.get_avail_by_job(&job).await?.is_none());

        // Found under any expiry, without the caller knowing it.
        let body = serde_json::json!({ "job_id": "job1", "expires_at": "never" });
        seed_avail(
            &todo_dir,
            &job,
            ExpiresAt::Never,
            &serde_json::to_vec(&body)?,
        )?;
        assert_eq!(store.get_avail_by_job(&job).await?, Some(body));

        // A different job's entry is not matched on a prefix overlap.
        assert!(
            store
                .get_avail_by_job(&JobId::new_unchecked("job"))
                .await?
                .is_none()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_deletes_are_idempotent_on_absent() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("nope");
        let client = ClientId::try_new("nobody")?;
        let when = dt("2026-06-15T08:30:00Z")?;

        // None of these exist; every delete must succeed as a no-op.
        store.delete_avail(&job, ExpiresAt::Never).await?;
        store
            .delete_lease(&job, ExpiresAt::Never, &client, when)
            .await?;
        store
            .delete_pending_reindex("nobody.absent-flag-key")
            .await?;
        store.delete_tmp_object("absent.json").await?;
        store.delete_suspension(&client).await?;
        store.delete_eligible_for_client(&client).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_suspension_round_trip() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let client = ClientId::try_new("client1")?;

        assert!(store.read_suspension(&client).await?.is_none());

        let at = dt("2026-06-17T09:00:00Z")?;
        let conflict = JobId::new_unchecked("jobconflict");
        store.write_suspension(&client, at, &conflict).await?;

        let SuspensionRecord {
            suspended_at,
            conflicting_job_id,
        } = store
            .read_suspension(&client)
            .await?
            .context("suspension")?;
        assert_eq!(suspended_at, at);
        assert_eq!(conflicting_job_id, conflict);

        let listed = store.list_suspensions().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, client);

        store.delete_suspension(&client).await?;
        assert!(store.read_suspension(&client).await?.is_none());
        assert!(store.list_suspensions().await?.is_empty());

        Ok(())
    }

    /// Both reads report the modification time of a stamp. A second stamp moves
    /// that time forward. A second delete also succeeds.
    #[tokio::test]
    async fn test_localfs_todo_liveness_round_trip() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let client = ClientId::try_new("client1")?;
        assert!(store.read_liveness(&client).await?.is_none());

        let before = Utc::now() - chrono::TimeDelta::seconds(1);
        store.stamp_liveness(&client).await?;
        let seen = store.read_liveness(&client).await?.context("stamped")?;
        assert!(seen >= before, "{seen} is before {before}");
        assert_eq!(store.list_liveness().await?, vec![(client.clone(), seen)]);

        // Backdate the marker, so that the second stamp must move the time.
        let marker = todo_dir.join("liveness").join("client1");
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&marker)?
            .set_modified(hour_ago)?;
        assert!(store.read_liveness(&client).await?.context("backdated")? < before);
        store.stamp_liveness(&client).await?;
        assert!(store.read_liveness(&client).await?.context("restamped")? >= before);

        // Foreign cruft is skipped, not an error.
        std::fs::write(todo_dir.join("liveness").join(".DS_Store"), b"")?;
        assert_eq!(store.list_liveness().await?.len(), 1);

        store.delete_liveness(&client).await?;
        store.delete_liveness(&client).await?;
        assert!(store.read_liveness(&client).await?.is_none());
        assert!(
            store
                .list_liveness()
                .await?
                .iter()
                .all(|(id, _)| id != &client)
        );

        // A todo directory without `liveness/` lists nothing.
        std::fs::remove_dir_all(todo_dir.join("liveness"))?;
        assert!(store.list_liveness().await?.is_empty());
        Ok(())
    }

    /// One malformed file must not hide every other suspended client.
    #[tokio::test]
    async fn test_localfs_todo_list_suspensions_skips_malformed() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let good = ClientId::try_new("good-client")?;
        store
            .write_suspension(
                &good,
                dt("2026-06-17T09:00:00Z")?,
                &JobId::new_unchecked("j"),
            )
            .await?;
        // A file that parses as a client id but holds invalid JSON.
        std::fs::write(
            todo_dir.join("suspended").join("bad-client.json"),
            b"{not json",
        )?;

        let listed = store.list_suspensions().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, good);

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_eligible_index() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let c1 = ClientId::try_new("c1")?;
        let c2 = ClientId::try_new("c2")?;
        let j1 = JobId::new_unchecked("j1");
        let j2 = JobId::new_unchecked("j2");
        let soon = ExpiresAt::At(dt("2026-06-23T00:00:00Z")?);

        assert!(store.list_eligible_for_client(&c1).await?.is_empty());
        assert!(store.list_all_eligible().await?.is_empty());

        // Marker carries the job's expiry; it round-trips through the filename.
        store.write_eligible(&c1, &j1, ExpiresAt::Never).await?;
        store.write_eligible(&c1, &j2, soon).await?;
        store.write_eligible(&c2, &j1, ExpiresAt::Never).await?;

        let mut for_c1 = store.list_eligible_for_client(&c1).await?;
        for_c1.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            for_c1,
            vec![(j1.clone(), ExpiresAt::Never), (j2.clone(), soon)]
        );
        assert_eq!(store.list_all_eligible().await?.len(), 3);

        // Deleting one client's markers leaves the others untouched.
        store.delete_eligible_for_client(&c1).await?;
        assert!(store.list_eligible_for_client(&c1).await?.is_empty());
        assert_eq!(
            store.list_all_eligible().await?,
            vec![(c2.clone(), j1.clone(), ExpiresAt::Never)]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_denied_round_trip() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("jobd");
        let ca = ClientId::try_new("ca")?;
        let cb = ClientId::try_new("cb")?;

        store
            .write_denied(&job, &ca, &DenialRecord::default())
            .await?;
        store
            .write_denied(&job, &cb, &DenialRecord::default())
            .await?;

        let mut denied: Vec<ClientId> = store
            .list_all_denied()
            .await?
            .into_iter()
            .filter(|(denied_job, _)| denied_job == &job)
            .map(|(_, client_id)| client_id)
            .collect();
        denied.sort();
        assert_eq!(denied, vec![ca.clone(), cb.clone()]);

        // The point read agrees with the listing, and addresses the pair: a
        // client that denied a different job is not denied on this one.
        let cc = ClientId::try_new("cc")?;
        assert!(store.is_denied(&job, &ca).await?);
        assert!(!store.is_denied(&job, &cc).await?);
        assert!(!store.is_denied(&JobId::new_unchecked("jobz"), &ca).await?);

        // The marker body round-trips. A second write replaces the record, and
        // an empty marker body reads as the default record.
        assert_eq!(store.get_denied(&job, &cc).await?, None);
        assert_eq!(
            store.get_denied(&job, &ca).await?,
            Some(DenialRecord::default())
        );
        let record = DenialRecord::new(Utc::now(), "runtime OOM", Some("1.4.2"), None);
        store.write_denied(&job, &ca, &record).await?;
        assert_eq!(store.get_denied(&job, &ca).await?, Some(record));
        std::fs::write(store.denied_path(&job, &cb), b"")?;
        assert_eq!(
            store.get_denied(&job, &cb).await?,
            Some(DenialRecord::default())
        );

        // `list_all_denied` splits `{job_id}.{client_id}` on `.`, so a `_` id
        // round-trips.
        let job2 = JobId::new_unchecked("jobe");
        let underscored = ClientId::try_new("ev1_abc")?;
        store
            .write_denied(&job2, &underscored, &DenialRecord::default())
            .await?;
        // A `write_denied` staging file is not a marker.
        std::fs::write(store.denied_dir.join(".tmp-staged"), b"{")?;

        let mut all = store.list_all_denied().await?;
        all.sort();
        let mut expected = vec![
            (job.clone(), ca.clone()),
            (job.clone(), cb.clone()),
            (job2.clone(), underscored.clone()),
        ];
        expected.sort();
        assert_eq!(all, expected);

        store.delete_denied_for_job(&job).await?;
        assert!(!store.is_denied(&job, &ca).await?);
        assert!(!store.is_denied(&job, &cb).await?);
        // The other job's marker is untouched.
        assert_eq!(store.list_all_denied().await?, vec![(job2, underscored)]);

        Ok(())
    }

    /// A denial read as absent hands the client back a job it reported failed,
    /// so any stat failure but `NotFound` must error (`try_exists`).
    #[tokio::test]
    async fn test_localfs_is_denied_propagates_a_non_notfound_error() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let denied_dir = todo_dir.join("denied");
        std::fs::remove_dir(&denied_dir)?;
        std::fs::write(&denied_dir, b"")?;

        let err = store
            .is_denied(&JobId::new_unchecked("jobd"), &ClientId::try_new("ca")?)
            .await;
        assert!(
            err.is_err(),
            "an unreadable denied/ must not read as 'not denied'"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_pending_reindex_round_trip() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        // The nonce splits off on `.`, outside the id charset, so a `_` id
        // round-trips.
        let client = ClientId::try_new("cx_1")?;

        assert!(store.list_pending_reindex().await?.is_empty());
        assert!(!store.has_pending_reindex(&client).await?);

        // Two writes mint two distinct keys for the same client.
        store.write_pending_reindex(&client).await?;
        store.write_pending_reindex(&client).await?;
        let flags = store.list_pending_reindex().await?;
        assert_eq!(flags.len(), 2);
        assert!(flags.iter().all(|(flagged, _)| flagged == &client));
        assert_ne!(flags[0].1, flags[1].1);
        assert!(store.has_pending_reindex(&client).await?);
        assert!(!store.has_pending_reindex(&ClientId::try_new("cx")?).await?);

        // Exact-key delete consumes one flag and must leave the other — the
        // property the reindex pass's capture-then-delete relies on.
        store.delete_pending_reindex(&flags[0].1).await?;
        assert!(store.has_pending_reindex(&client).await?);
        // Idempotent: the key is already gone.
        store.delete_pending_reindex(&flags[0].1).await?;
        assert_eq!(store.list_pending_reindex().await?, vec![flags[1].clone()]);

        store.delete_pending_reindex(&flags[1].1).await?;
        assert!(store.list_pending_reindex().await?.is_empty());
        assert!(!store.has_pending_reindex(&client).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_pending_reindex_jobs_round_trip() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job-1");
        // Foreign cruft is skipped, not an error.
        std::fs::write(todo_dir.join("pending-reindex-jobs").join(".DS_Store"), b"")?;

        assert!(store.list_pending_reindex_jobs().await?.is_empty());
        store.write_pending_reindex_job(&job).await?;
        assert_eq!(store.list_pending_reindex_jobs().await?, vec![job.clone()]);
        store.delete_pending_reindex_job(&job).await?;
        assert!(store.list_pending_reindex_jobs().await?.is_empty());
        Ok(())
    }

    /// A loss is keyed by its lease, so a repeat write adds nothing; foreign
    /// names are skipped, and the listing is oldest first.
    #[tokio::test]
    async fn test_localfs_todo_lost_leases_round_trip() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job-1");
        let (a, b) = (ClientId::try_new("ev1_a")?, ClientId::try_new("ev1_b")?);
        let (early, late) = (dt("2026-10-03T04:50:00Z")?, dt("2026-10-03T05:00:00Z")?);
        assert!(store.list_lost_leases(&job).await?.is_empty());
        assert!(store.list_lost_lease_jobs().await?.is_empty());

        store.write_lost_lease(&job, &b, late).await?;
        store.write_lost_lease(&job, &a, early).await?;
        store.write_lost_lease(&job, &a, early).await?;
        std::fs::write(todo_dir.join("lost/job-1/.DS_Store"), b"")?;
        std::fs::write(todo_dir.join("lost/.DS_Store"), b"")?;

        assert_eq!(
            store.list_lost_leases(&job).await?,
            vec![(a, early), (b, late)]
        );
        assert_eq!(store.list_lost_lease_jobs().await?, vec![job.clone()]);
        store.delete_lost_leases_for_job(&job).await?;
        store.delete_lost_leases_for_job(&job).await?;
        assert!(store.list_lost_leases(&job).await?.is_empty());
        assert!(store.list_lost_lease_jobs().await?.is_empty());
        Ok(())
    }

    /// The persisted `queue-maintenance` key sets, so one test covers all three.
    #[derive(Debug, Clone, Copy)]
    enum KeySet {
        EligibleIndexed,
        EligibleWorkers,
        GcCandidates,
    }

    impl KeySet {
        fn file(self) -> &'static str {
            match self {
                Self::EligibleIndexed => ".eligible-indexed",
                Self::EligibleWorkers => ".eligible-workers",
                Self::GcCandidates => ".gc-candidates",
            }
        }

        async fn write(self, store: &LocalFsTodoStore, items: &[&str]) -> anyhow::Result<()> {
            let strings: HashSet<String> = items.iter().map(|s| s.to_string()).collect();
            match self {
                Self::EligibleIndexed => store.write_eligible_indexed(&strings).await,
                Self::EligibleWorkers => {
                    let ids = items
                        .iter()
                        .map(|s| ClientId::try_new(*s))
                        .collect::<Result<HashSet<_>, _>>()?;
                    store.write_eligible_workers(&ids).await
                }
                Self::GcCandidates => store.write_gc_candidates(&strings).await,
            }
        }

        async fn read(self, store: &LocalFsTodoStore) -> anyhow::Result<HashSet<String>> {
            Ok(match self {
                Self::EligibleIndexed => store.read_eligible_indexed().await?,
                Self::EligibleWorkers => store
                    .read_eligible_workers()
                    .await?
                    .iter()
                    .map(|c| c.as_str().to_owned())
                    .collect(),
                Self::GcCandidates => store.read_gc_candidates().await?,
            })
        }
    }

    /// Missing or corrupt state reads as empty; a written set round-trips.
    #[rstest]
    #[case::eligible_indexed(
        KeySet::EligibleIndexed,
        &["job-a.never.json", "job-b.1735689600.json"]
    )]
    #[case::eligible_workers(KeySet::EligibleWorkers, &["ev1_a", "ev1_b"])]
    #[case::gc_candidates(
        KeySet::GcCandidates,
        &["eligible/clients/client-a/job-a.never", "denied/job-b.client-b"]
    )]
    #[tokio::test]
    async fn test_localfs_todo_key_set_round_trip(
        #[case] set: KeySet,
        #[case] sample: &[&str],
    ) -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;

        assert!(set.read(&store).await?.is_empty());

        set.write(&store, sample).await?;
        let expected: HashSet<String> = sample.iter().map(|s| s.to_string()).collect();
        assert_eq!(set.read(&store).await?, expected);

        std::fs::write(todo_dir.join(set.file()), b"not json")?;
        assert!(set.read(&store).await?.is_empty());

        Ok(())
    }

    /// Missing or corrupt snapshots read as `None`.
    #[tokio::test]
    async fn test_localfs_todo_backlog_round_trip() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        assert_eq!(store.read_backlog().await?, None);

        let snapshot = BacklogSnapshot {
            counted_at: DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?,
            jobs: [(ClientId::try_new("ev1_a")?, 3)].into_iter().collect(),
        };
        store.write_backlog(&snapshot).await?;
        assert_eq!(store.read_backlog().await?, Some(snapshot));

        std::fs::write(todo_dir.join(".backlog"), b"not json")?;
        assert_eq!(store.read_backlog().await?, None);
        Ok(())
    }

    /// Missing or corrupt snapshots read as `None`.
    #[tokio::test]
    async fn test_localfs_todo_campaign_backlog_round_trip() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        assert_eq!(store.read_campaign_backlog().await?, None);

        let snapshot = CampaignBacklogSnapshot {
            counted_at: DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?,
            devices: [(
                ClientId::try_new("ev1_a")?,
                CampaignBacklog {
                    by_campaign: [(
                        CampaignId::try_new("campaign_00000000-0000-0000-0000-000000000001")?,
                        2,
                    )]
                    .into_iter()
                    .collect(),
                    outside_campaigns: 1,
                },
            )]
            .into_iter()
            .collect(),
        };
        store.write_campaign_backlog(&snapshot).await?;
        assert_eq!(store.read_campaign_backlog().await?, Some(snapshot));

        std::fs::write(todo_dir.join(".backlog-campaigns"), b"not json")?;
        assert_eq!(store.read_campaign_backlog().await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_list_stale_tmp() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        std::fs::write(todo_dir.join("tmp").join("leftover.json"), b"{}")?;

        // Age 0 → everything is at least that old.
        assert_eq!(
            store.list_stale_tmp(Duration::ZERO).await?,
            vec!["leftover.json".to_string()]
        );
        // A just-written file is not yet an hour old.
        assert!(
            store
                .list_stale_tmp(Duration::from_secs(3600))
                .await?
                .is_empty()
        );

        store.delete_tmp_object("leftover.json").await?;
        assert!(store.list_stale_tmp(Duration::ZERO).await?.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_build_local_fs_stores_use_a_separate_auth_dir() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let auth_dir = tempfile::tempdir()?;

        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            storage: pipette_core::config::StorageConfig::local_fs(data_dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(
                data_dir.path().to_path_buf(),
            ),
            ..Config::default()
        };
        let mut stores = build_local_fs_stores(&config)?;

        let auth_storage =
            pipette_core::config::StorageConfig::local_fs(auth_dir.path().to_path_buf());
        stores.clients = super::build_local_fs_client_store(&auth_storage)?;
        stores.users = super::build_local_fs_user_store(&auth_storage)?;

        let public_key = pipette_plan_types::PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let c = Client {
            client_id: ClientId::derive(&public_key),
            public_key: public_key.clone(),
            organization: pipette_plan_types::NonEmptyTrimmedString::try_new("test")?,
            client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("d")?,
            contact_email: pipette_plan_types::ContactEmail::try_new("t@t.com")?,
            status: pipette_core::client::ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };
        stores.clients.put_client(&c).await?;

        // Client file should be in auth_dir, not data_dir
        assert!(
            auth_dir
                .path()
                .join("clients")
                .join(format!("{}.json", c.client_id))
                .exists()
        );
        assert!(
            !data_dir
                .path()
                .join("clients")
                .join(format!("{}.json", c.client_id))
                .exists()
        );

        let loaded = stores
            .clients
            .get_client(&c.client_id)
            .await?
            .context("the separate auth store must hold the client")?;
        assert_eq!(loaded.client_id, c.client_id);

        // The user store is rooted at the same override, so an account lands
        // beside the client records rather than back in `[storage]`.
        let user = pipette_core::user::User::try_new(
            pipette_plan_types::ContactEmail::try_new("op@example.com")?,
            Utc::now(),
        )?;
        stores.users.put_user(&user).await?;
        assert!(
            auth_dir
                .path()
                .join("users")
                .join(format!("{}.json", user.user_id))
                .exists()
        );
        assert!(
            !data_dir.path().join("users").exists(),
            "the overridden user store must not write under [storage]"
        );

        Ok(())
    }
}
