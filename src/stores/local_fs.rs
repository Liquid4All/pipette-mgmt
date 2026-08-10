use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::benchmark::{self, Benchmark};
use crate::client::{self, Client};
use crate::config::Config;
use crate::eval_sample_result::{self, EvalSampleResult};
use crate::parquet_utils::WriterOpts;
use crate::plan::{PlanManifest, PlanStatus};
use crate::preauth::{
    PreauthConsumeOutcome, PreauthKey, PreauthRejection, PreauthUsage, Secret, validate,
};
use crate::storage;
use crate::stores::{
    AuthStore, CatalogStore, ClaimResult, EvalSampleResultStore, JobState, PlanStore,
    RecycleResult, RenewLeaseResult, ScoreQueueStage, Stores, SubmissionRecord, SubmissionStore,
    SuspensionRecord, TodoStore, WarehouseStore,
};
use crate::todo_filename::{
    avail_filename, eligible_filename, leased_key, parse_denied_marker, parse_eligible_filename,
    tmp_filename,
};
use crate::types::{BenchmarkId, ClientId, ExpiresAt, JobId, PlanId, PreauthKeyId};
use crate::warehouse::{self, JobMetrics, MetricRow};

pub fn build_local_fs_auth_store(
    storage: &crate::config::StorageConfig,
) -> anyhow::Result<Arc<dyn AuthStore>> {
    let data_dir = storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_auth_store requires local_fs backend"))?;
    let clients_dir = data_dir.join("clients");
    std::fs::create_dir_all(&clients_dir)?;
    Ok(Arc::new(LocalFsAuthStore::new(clients_dir)))
}

/// Build a complete local_fs `Stores` rooted at `[storage]`. The `todo` store is
/// also rooted at `[storage]`: `[todo_storage]` is honored only via
/// `build_stores` in the binary, so direct callers (e.g. tests) ignore it.
pub fn build_local_fs_stores(config: &Config) -> anyhow::Result<Stores> {
    let data_dir = config
        .storage
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("build_local_fs_stores requires local_fs backend"))?;

    let writer_opts = config.writer_opts();

    let benchmarks_dir = data_dir.join("benchmarks");
    let clients_dir = data_dir.join("clients");
    let submissions_dir = data_dir.join("submissions");
    let warehouse_dir = data_dir.join("warehouse");
    let warehouse_results_dir = warehouse_dir.join("results");
    let eval_sample_results_dir = warehouse_dir.join("eval_sample_results");
    let plans_dir = data_dir.join("plans");
    let cancelled_plans_dir = data_dir.join("cancelled_plans");

    std::fs::create_dir_all(&clients_dir)?;
    std::fs::create_dir_all(submissions_dir.join("incoming"))?;
    std::fs::create_dir_all(submissions_dir.join("processed"))?;
    std::fs::create_dir_all(&warehouse_results_dir)?;
    std::fs::create_dir_all(&eval_sample_results_dir)?;
    std::fs::create_dir_all(&plans_dir)?;
    std::fs::create_dir_all(&cancelled_plans_dir)?;

    Ok(Stores {
        catalog: Arc::new(LocalFsCatalogStore::new(benchmarks_dir)),
        auth: Arc::new(LocalFsAuthStore::new(clients_dir)),
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
        // Seed `todo` from this `[storage]` backend so direct callers (e.g.
        // tests) get a working store; `build_stores` replaces it with the
        // `config.todo_storage()`-resolved store.
        todo: build_local_fs_todo_store(&config.storage)?,
        plans: Arc::new(LocalFsPlanStore::new(plans_dir, cancelled_plans_dir)),
    })
}

/// Build a `local_fs` `TodoStore`, creating the `todo/` subtree. `storage` must
/// be a local_fs backend.
pub fn build_local_fs_todo_store(
    storage: &crate::config::StorageConfig,
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
    std::fs::create_dir_all(todo_dir.join("tmp"))?;
    std::fs::create_dir_all(todo_dir.join("suspended"))?;
    Ok(Arc::new(LocalFsTodoStore::new(todo_dir)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn run_blocking<T, F>(f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))?
}

/// Remove a file, treating an already-absent file as success. Used by callers
/// that route a record out of one directory after copying it elsewhere (so the
/// remove must be idempotent on retry) and by the queue stores that delete
/// transient markers which may already be gone.
async fn remove_file_idempotent(path: PathBuf) -> anyhow::Result<()> {
    run_blocking(move || match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    })
    .await
}

// ---------------------------------------------------------------------------
// CatalogStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocalFsCatalogStore {
    benchmarks_dir: PathBuf,
}

impl LocalFsCatalogStore {
    pub fn new(benchmarks_dir: PathBuf) -> Self {
        Self { benchmarks_dir }
    }
}

#[async_trait]
impl CatalogStore for LocalFsCatalogStore {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let benchmarks_dir = self.benchmarks_dir.clone();
        run_blocking(move || benchmark::load_catalog(&benchmarks_dir)).await
    }
}

// ---------------------------------------------------------------------------
// AuthStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocalFsAuthStore {
    clients_dir: PathBuf,
    /// Forward tag index (`tags-index/by-client/{client_id}/{tag}` markers).
    by_client_dir: PathBuf,
    /// Reverse tag index (`tags-index/by-tag/{tag}/{client_id}` markers).
    by_tag_dir: PathBuf,
    /// Pre-auth key records (`preauth/{key_id}.json`).
    preauth_dir: PathBuf,
    /// Signature migration markers (`signature-migration/{client_id}.json`).
    signature_migration_dir: PathBuf,
}

impl LocalFsAuthStore {
    pub fn new(clients_dir: PathBuf) -> Self {
        // The tag indexes live under a `tags-index/` root beside `clients/`
        // (i.e. under clients_dir's parent), kept separate from the record dir
        // so listing records never enumerates tag markers. Fall back to nesting
        // under clients_dir if there is no parent (never the case for real
        // roots), so the paths are always well-defined.
        let root = clients_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| clients_dir.clone());
        let index = root.join("tags-index");
        Self {
            preauth_dir: root.join("preauth"),
            signature_migration_dir: root.join("signature-migration"),
            clients_dir,
            by_client_dir: index.join("by-client"),
            by_tag_dir: index.join("by-tag"),
        }
    }
}

#[async_trait]
impl AuthStore for LocalFsAuthStore {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>> {
        let clients_dir = self.clients_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client::load_client(&clients_dir, &client_id)).await
    }

    async fn put_client(&self, client: &Client) -> anyhow::Result<()> {
        let clients_dir = self.clients_dir.clone();
        let client = client.clone();
        run_blocking(move || client::save_client(&clients_dir, &client)).await
    }

    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let clients_dir = self.clients_dir.clone();
        let by_client_dir = self.by_client_dir.clone();
        let by_tag_dir = self.by_tag_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || {
            client::delete_client(&clients_dir, &client_id)?;
            client::delete_all_tags(&by_client_dir, &by_tag_dir, &client_id)
        })
        .await
    }

    async fn list_clients(&self) -> anyhow::Result<Vec<Client>> {
        let clients_dir = self.clients_dir.clone();
        run_blocking(move || client::list_all(&clients_dir)).await
    }

    async fn has_public_key(
        &self,
        public_key: &crate::validated::PublicKeyHex,
    ) -> anyhow::Result<bool> {
        let clients_dir = self.clients_dir.clone();
        let public_key = public_key.clone();
        run_blocking(move || client::find_by_public_key(&clients_dir, &public_key)).await
    }

    async fn add_client_tag(
        &self,
        client_id: &ClientId,
        tag: &crate::validated::Tag,
    ) -> anyhow::Result<()> {
        let by_client_dir = self.by_client_dir.clone();
        let by_tag_dir = self.by_tag_dir.clone();
        let client_id = client_id.clone();
        let tag = tag.clone();
        run_blocking(move || client::add_tag(&by_client_dir, &by_tag_dir, &client_id, &tag)).await
    }

    async fn remove_client_tag(
        &self,
        client_id: &ClientId,
        tag: &crate::validated::Tag,
    ) -> anyhow::Result<()> {
        let by_client_dir = self.by_client_dir.clone();
        let by_tag_dir = self.by_tag_dir.clone();
        let client_id = client_id.clone();
        let tag = tag.clone();
        run_blocking(move || client::remove_tag(&by_client_dir, &by_tag_dir, &client_id, &tag))
            .await
    }

    async fn get_client_tags(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<std::collections::BTreeSet<crate::validated::Tag>> {
        let by_client_dir = self.by_client_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client::list_client_tags(&by_client_dir, &client_id)).await
    }

    async fn list_client_ids_by_tag(
        &self,
        tag: &crate::validated::Tag,
    ) -> anyhow::Result<Vec<ClientId>> {
        let by_tag_dir = self.by_tag_dir.clone();
        let tag = tag.clone();
        run_blocking(move || client::list_client_ids_by_tag(&by_tag_dir, &tag)).await
    }

    async fn list_forward_tag_markers(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, crate::validated::Tag)>> {
        let by_client_dir = self.by_client_dir.clone();
        run_blocking(move || client::list_all_forward_markers(&by_client_dir)).await
    }

    async fn list_reverse_tag_markers(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, crate::validated::Tag)>> {
        let by_tag_dir = self.by_tag_dir.clone();
        run_blocking(move || client::list_all_reverse_markers(&by_tag_dir)).await
    }

    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        let dir = self.signature_migration_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client::has_signature_migration(&dir, &client_id)).await
    }

    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<crate::client::MigrationRecord> {
        let dir = self.signature_migration_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client::record_signature_migration(&dir, &client_id, at)).await
    }

    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, crate::client::SignatureMigration)>> {
        let dir = self.signature_migration_dir.clone();
        run_blocking(move || client::list_signature_migrations(&dir)).await
    }

    async fn put_preauth_key(&self, key: &PreauthKey) -> anyhow::Result<()> {
        let dir = self.preauth_dir.clone();
        let key = key.clone();
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.json", key.key_id));
            std::fs::write(path, serde_json::to_vec_pretty(&key)?)?;
            Ok(())
        })
        .await
    }

    async fn consume_preauth_key(
        &self,
        key_id: &PreauthKeyId,
        secret: &Secret,
    ) -> anyhow::Result<PreauthConsumeOutcome> {
        let path = self.preauth_dir.join(format!("{key_id}.json"));
        let marker = self.preauth_dir.join(format!("{key_id}.spent"));
        let secret = secret.clone();
        run_blocking(move || {
            let data = match std::fs::read(&path) {
                Ok(data) => data,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(PreauthConsumeOutcome::Rejected(PreauthRejection::NotFound));
                }
                Err(e) => return Err(e.into()),
            };
            let now = Utc::now();
            let key: PreauthKey = serde_json::from_slice(&data)?;
            let grant = match validate(&key, &secret, now) {
                Ok(grant) => grant,
                Err(rejection) => return Ok(PreauthConsumeOutcome::Rejected(rejection)),
            };
            // Spending is the exclusive create of the marker, not the delete of
            // the record: `create_new` is `O_EXCL`, so one of any number of
            // concurrent consumes creates it and the rest see `AlreadyExists`.
            // The record delete that follows is cleanup — if it never lands, the
            // marker still stands and the next consume loses the create.
            // Multi-use keys are not mutated, so nothing here runs for them.
            if matches!(key.usage, PreauthUsage::SingleUse) {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&marker)
                {
                    Ok(mut file) => {
                        std::io::Write::write_all(&mut file, now.to_rfc3339().as_bytes())?
                    }
                    // Already spent. Reported as unknown, like every other
                    // reason a key will not grant.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Ok(PreauthConsumeOutcome::Rejected(PreauthRejection::NotFound));
                    }
                    Err(e) => return Err(e.into()),
                }
                // Cleanup, so a record already gone (an operator revoking in
                // parallel) is the outcome wanted rather than a failure.
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(PreauthConsumeOutcome::Granted(grant))
        })
        .await
    }

    async fn list_preauth_keys(&self) -> anyhow::Result<Vec<PreauthKey>> {
        let dir = self.preauth_dir.clone();
        run_blocking(move || {
            if !dir.exists() {
                return Ok(Vec::new());
            }
            // Skip non-`.json` entries (`Ok(None)`) but let read/parse errors
            // propagate; `transpose` + `filter_map` drops the Nones and keeps
            // both successes and errors so `collect` can surface the latter.
            std::fs::read_dir(&dir)?
                .map(|entry| -> anyhow::Result<Option<PreauthKey>> {
                    let path = entry?.path();
                    if path.extension().is_none_or(|ext| ext != "json") {
                        return Ok(None);
                    }
                    Ok(Some(serde_json::from_slice(&std::fs::read(&path)?)?))
                })
                .filter_map(Result::transpose)
                .collect::<anyhow::Result<Vec<PreauthKey>>>()
        })
        .await
    }

    async fn delete_preauth_key(&self, key_id: &PreauthKeyId) -> anyhow::Result<()> {
        let path = self.preauth_dir.join(format!("{key_id}.json"));
        run_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list_spent_markers(&self) -> anyhow::Result<Vec<PreauthKeyId>> {
        let dir = self.preauth_dir.clone();
        run_blocking(move || {
            if !dir.exists() {
                return Ok(Vec::new());
            }
            Ok(std::fs::read_dir(&dir)?
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let path = entry.path();
                    let name = path.file_name()?.to_str()?;
                    PreauthKeyId::try_new(name.strip_suffix(".spent")?).ok()
                })
                .collect())
        })
        .await
    }

    async fn delete_spent_marker(&self, key_id: &PreauthKeyId) -> anyhow::Result<()> {
        let path = self.preauth_dir.join(format!("{key_id}.spent"));
        run_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// PlanStore
// ---------------------------------------------------------------------------

/// Plan manifests as `{plans_dir}/{plan_id}.json`. Mirrors the preauth
/// durable-record pattern; the flat layout keeps `get_plan`/`delete_plan`
/// addressable by `plan_id` alone (see `docs/plan-ingestion.md` §9).
///
/// Cancel markers live in a separate `{cancelled_dir}` — a sibling of
/// `{plans_dir}`, not a file inside it, so `list_plans`'s `read_dir` never sees
/// them.
#[derive(Clone)]
pub struct LocalFsPlanStore {
    plans_dir: PathBuf,
    cancelled_dir: PathBuf,
}

impl LocalFsPlanStore {
    pub fn new(plans_dir: PathBuf, cancelled_dir: PathBuf) -> Self {
        Self {
            plans_dir,
            cancelled_dir,
        }
    }

    fn manifest_path(&self, plan_id: &PlanId) -> PathBuf {
        self.plans_dir.join(format!("{}.json", plan_id.as_str()))
    }

    /// Marker path — the bare `plan_id`, no extension, mirroring the other
    /// empty-marker trees (`todo/denied/`, `tags-index/`).
    fn cancel_marker_path(&self, plan_id: &PlanId) -> PathBuf {
        self.cancelled_dir.join(plan_id.as_str())
    }
}

#[async_trait]
impl PlanStore for LocalFsPlanStore {
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
        let dir = self.plans_dir.clone();
        let path = self.manifest_path(&manifest.plan_id);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Atomic replace: write a temp sibling then rename, so a concurrent
            // reader (`plans status`/`list_plans` racing the maintenance
            // rewrite) never observes a truncated manifest — matching the S3
            // backend's all-or-nothing PUT. The `.tmp-` name has no `.json`
            // extension, so `list_plans` skips it; the guard reaps it if the
            // write fails before the rename (mirrors the parquet write path).
            let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
            let guard = storage::TmpFileGuard::new(tmp.clone());
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

    async fn list_plans(&self, status: Option<PlanStatus>) -> anyhow::Result<Vec<PlanManifest>> {
        let dir = self.plans_dir.clone();
        run_blocking(move || {
            if !dir.exists() {
                return Ok(Vec::new());
            }
            // Skip non-`.json` entries (`Ok(None)`) but let read/parse errors
            // propagate — same shape as `list_preauth_keys`.
            let plans = std::fs::read_dir(&dir)?
                .map(|entry| -> anyhow::Result<Option<PlanManifest>> {
                    let path = entry?.path();
                    if path.extension().is_none_or(|ext| ext != "json") {
                        return Ok(None);
                    }
                    match std::fs::read(&path) {
                        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
                        // Removed between `read_dir` and here (e.g. the retention
                        // GC) — equivalent to having listed a moment later; skip.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                        Err(e) => Err(e.into()),
                    }
                })
                .filter_map(Result::transpose)
                .collect::<anyhow::Result<Vec<PlanManifest>>>()?;
            Ok(match status {
                Some(s) => plans.into_iter().filter(|p| p.status == s).collect(),
                None => plans,
            })
        })
        .await
    }

    async fn delete_plan(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        remove_file_idempotent(self.manifest_path(plan_id)).await
    }

    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        let dir = self.cancelled_dir.clone();
        let path = self.cancel_marker_path(plan_id);
        run_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            // Empty marker, and a plain write rather than write-tmp-then-rename:
            // there is no content to tear, so a reader can only ever see the
            // file present or absent.
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
            // An absent directory means no plan has ever been cancelled.
            // `try_exists`, not `exists`, for the same reason as
            // `has_cancel_marker`: `exists` folds an IO failure into `false`,
            // which teardown would read as "nothing to cancel" on every pass.
            if !dir.try_exists()? {
                return Ok(Vec::new());
            }
            // Entry read errors propagate (teardown must not silently lose a
            // cancel); an unparseable *name* is foreign cruft — warned and
            // skipped, matching `list_pending_reindex`.
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
}

impl LocalFsSubmissionStore {
    /// All submission state lives under one `submissions_dir` root; each
    /// subdirectory is derived from it explicitly (no path is inferred from
    /// a sibling's parent).
    pub fn new(submissions_dir: PathBuf) -> Self {
        Self {
            incoming_dir: submissions_dir.join("incoming"),
            processed_dir: submissions_dir.join("processed"),
            unverified_dir: submissions_dir.join("unverified"),
            score_queue_dir: submissions_dir.join(ScoreQueueStage::ROOT),
        }
    }

    /// Directory for a score-queue stage, e.g.
    /// `<submissions>/score-queue/to_do`.
    fn stage_dir(&self, stage: ScoreQueueStage) -> PathBuf {
        self.score_queue_dir.join(stage.leaf())
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

    async fn list_incoming(&self, limit: std::num::NonZeroUsize) -> anyhow::Result<Vec<JobId>> {
        let incoming_dir = self.incoming_dir.clone();
        run_blocking(move || storage::list_submission_job_ids(&incoming_dir, limit)).await
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

    async fn list_queue(
        &self,
        stage: ScoreQueueStage,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let dir = self.stage_dir(stage);
        run_blocking(move || storage::list_submission_job_ids(&dir, limit)).await
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

    async fn write_unverified(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let client_dir = self.unverified_dir.join(client_id.as_str());
        let job_id = job_id.clone();
        let body = body.clone();
        // Same atomic `{dir}/{job_id}.json` write as `incoming/`; only
        // the target directory (`unverified/{client_id}/`) differs.
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
    ) -> anyhow::Result<crate::stores::PruneSummary> {
        let unverified_dir = self.unverified_dir.clone();
        run_blocking(move || {
            let (deleted, kept) =
                storage::prune_unverified_dir(&unverified_dir, older_than, dry_run)?;
            Ok(crate::stores::PruneSummary { deleted, kept })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// WarehouseStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocalFsWarehouseStore {
    warehouse_dir: PathBuf,
    read_days: u32,
    max_rows_per_part: usize,
    writer_opts: WriterOpts,
}

impl LocalFsWarehouseStore {
    pub fn new(
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
        // Sync I/O: maintenance-only path with no latency budget, and
        // it sidesteps the lifetime gymnastics of moving a borrowed
        // `&mut dyn FnMut` into `spawn_blocking`.
        let mut stack = vec![self.warehouse_dir.clone()];
        while let Some(dir) = stack.pop() {
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                // Empty warehouse — nothing to walk.
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
                let mut rows = warehouse::rows_from_parquet_bytes(&std::fs::read(&path)?)?;
                if rows.is_empty() {
                    continue;
                }
                // `for_each` (not `any`) so every row sees `f` — `any`
                // short-circuits on the first `true` and would skip later
                // rows.
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
                let tmp = parent.join(format!(".tmp-{}.parquet", uuid::Uuid::new_v4()));
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, &path)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EvalSampleResultStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocalFsEvalSampleResultStore {
    base_dir: PathBuf,
    writer_opts: WriterOpts,
}

impl LocalFsEvalSampleResultStore {
    pub fn new(base_dir: PathBuf, writer_opts: WriterOpts) -> Self {
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
            // Atomic write: write to temp file then rename. The
            // `TmpFileGuard` removes the temp file on any early error
            // so a failed `write_parquet` doesn't leak an orphan into
            // the flat eval_sample_results dir.
            let tmp_path = base_dir.join(format!(".tmp-{}.parquet", uuid::Uuid::new_v4()));
            let guard = storage::TmpFileGuard::new(tmp_path.clone());
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
                // The dir is created lazily on first write; an absent dir just
                // means no eval sample results yet.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            let mut job_ids = Vec::new();
            for entry in read {
                let path = entry?.path();
                // Only `{job_id}.parquet`; skip the `.tmp-*.parquet` files an
                // interrupted atomic write may have left behind, and anything
                // whose stem isn't a valid job id.
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
    tmp_dir: PathBuf,
    suspended_dir: PathBuf,
    cursor_path: PathBuf,
    gc_candidates_path: PathBuf,
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
            tmp_dir: todo_dir.join("tmp"),
            suspended_dir: todo_dir.join("suspended"),
            cursor_path: todo_dir.join(".eligible-cursor"),
            gc_candidates_path: todo_dir.join(".gc-candidates"),
        }
    }

    /// Path to a client's `suspended/{client_id}.json` marker.
    fn suspension_path(&self, client_id: &ClientId) -> PathBuf {
        self.suspended_dir.join(format!("{}.json", client_id))
    }
}

#[async_trait]
impl TodoStore for LocalFsTodoStore {
    async fn list_avail(
        &self,
        start_after: Option<&str>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<String>> {
        let avail_dir = self.avail_dir.clone();
        let start_after = start_after.map(str::to_owned);
        run_blocking(move || {
            // This listing feeds the queue-maintenance GC live set and the
            // eligible-index cursor, where a *negative* ("job not in avail/")
            // drives marker deletion and permanent index skips, so a swallowed
            // per-entry read error that hid a real job would be destructive —
            // the asymmetric-negative case documented on `renew_lease` and
            // `list_leased`. Every read error propagates rather than silently
            // shrinking the answer; non-UTF-8 and non-`.json` names are
            // foreign cruft, not jobs, and are soundly dropped.
            let mut names: Vec<String> = std::fs::read_dir(&avail_dir)?
                .map(|e| -> anyhow::Result<Option<String>> {
                    Ok(e?
                        .file_name()
                        .into_string()
                        .ok()
                        .filter(|n| n.ends_with(".json")))
                })
                .filter_map(|r| r.transpose())
                .collect::<anyhow::Result<_>>()?;
            names.sort_unstable();
            if let Some(after) = start_after {
                names.retain(|n| n.as_str() > after.as_str());
            }
            names.truncate(limit.get());
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
        remove_file_idempotent(self.avail_dir.join(avail_filename(job_id, expires_at))).await
    }

    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let avail_dir = self.avail_dir.clone();
        let prefix = format!("{}.", job_id);
        run_blocking(move || {
            // Per-entry `?` propagates rather than swallowing: a read error
            // mistaken for "absent" would leave an `avail/` file behind while
            // reporting the job torn down (the asymmetric-negative case in
            // CLAUDE.md), so an unreadable dir must fail the whole delete.
            std::fs::read_dir(&avail_dir)?.try_for_each(|entry| {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && name.ends_with(".json") {
                    std::fs::remove_file(entry.path())?;
                }
                Ok(())
            })
        })
        .await
    }

    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let avail_dir = self.avail_dir.clone();
        let prefix = format!("{}.", job_id);
        run_blocking(move || {
            // A read error here must not masquerade as "absent": treating an
            // unreadable dir as None would skip a legitimate escalation (the
            // asymmetric-negative case in CLAUDE.md), so the top-level read and
            // the matched file read both propagate via `?`.
            for entry in std::fs::read_dir(&avail_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && name.ends_with(".json") {
                    let body = serde_json::from_str(&std::fs::read_to_string(entry.path())?)?;
                    return Ok(Some(body));
                }
            }
            Ok(None)
        })
        .await
    }

    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        let leased_dir = self.leased_dir.clone();
        run_blocking(move || {
            // leased/{client_id}/{job_id}.{expiry}.json — descend each client
            // partition and return full `{client_id}/{leaf}` relative keys.
            //
            // This listing feeds the queue-maintenance GC live set, where a
            // *negative* ("job not leased anywhere") drives marker deletion, so
            // a swallowed read error that hid a real lease would be destructive
            // — the asymmetric-negative case documented on `renew_lease`. Only a
            // partition that vanished mid-scan (NotFound) is soundly "nothing
            // here"; every other read error propagates rather than silently
            // shrinking the answer. A genuine non-directory entry under
            // `leased/` is not a partition and contributes nothing, but an
            // entry whose type we cannot read propagates rather than being
            // guessed absent.
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
                                Ok(leaf
                                    .ends_with(".json")
                                    .then(|| format!("{client_name}/{leaf}")))
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
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let path = self
            .leased_dir
            .join(leased_key(job_id, client_id, lease_expiry));
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
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".json"))
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

            // 1. Locate this client's current lease for the job (at most one)
            //    by scanning only its own partition — the filename carries the
            //    expiry the rename source needs. The renewal predicate is lease
            //    *existence*, not its expiry: an expired-but-not-yet-recycled
            //    lease still renews. That's intentional — recycling is the only
            //    thing that ends a lease, and it's atomic w.r.t. this rename
            //    (step 2 tolerates losing that race). Do not add an expiry check
            //    here; it would race the recycler.
            let own_dir = leased_dir.join(client_id.as_str());
            let own_leaf = match std::fs::read_dir(&own_dir) {
                Ok(entries) => entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                    .find(|n| n.to_string_lossy().starts_with(&job_prefix)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };

            // 2. If found, atomically rename it to the new expiry.
            if let Some(leaf) = own_leaf {
                let src = own_dir.join(&leaf);
                let dst = leased_dir.join(leased_key(&job_id, &client_id, new_expiry));
                match std::fs::rename(&src, &dst) {
                    Ok(()) => return Ok(RenewLeaseResult::Renewed),
                    // Raced with a recycle between the scan and the rename;
                    // fall through to the cross-partition check below.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }

            // 3. This client holds no lease for the job. Distinguish "gone"
            //    (NotFound) from "held by someone else" (WrongClient) by
            //    scanning the other client partitions — only on this unhappy
            //    path.
            //
            // Soundness note — why this is NOT the usual fully-lenient chain:
            // the answer is asymmetric. *Finding* the job in a partition we
            // could read is always conclusive (→ WrongClient). *Not* finding it
            // is only conclusive if we actually scanned every partition. So a
            // swallowed read error would silently downgrade a true WrongClient
            // (409) to NotFound (404) whenever the job lives in the one
            // partition we couldn't see — and 404-vs-409 is load-bearing in the
            // API contract (httpapi.md §2.10). We therefore split the inner
            // error kinds rather than blanket-swallowing:
            //   - NotFound: the partition dir vanished, so that client holds no
            //     leases at all → soundly contributes "not here" (false).
            //   - any other error (permissions, EIO, EMFILE, …): the dir exists
            //     and might hold our job, but we can't look → we genuinely
            //     cannot answer, so propagate rather than fabricate a 404.
            // This is why a plain `.any()` over `into_iter().flatten()` would be wrong
            // here even though it reads cleaner; don't "simplify" it back.
            let wrong_client = std::fs::read_dir(&leased_dir)?
                .filter_map(|e| e.ok())
                .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
                // Our own partition was already checked in step 1.
                .filter(|entry| entry.file_name().to_string_lossy() != client_id.as_str())
                .try_fold(false, |found, entry| -> anyhow::Result<bool> {
                    // Already found a holder: skip the remaining (now pointless)
                    // partition reads. `found` is sticky once true.
                    if found {
                        return Ok(true);
                    }
                    let held = match std::fs::read_dir(entry.path()) {
                        Ok(leaves) => leaves
                            .filter_map(|e| e.ok())
                            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                            .any(|e| e.file_name().to_string_lossy().starts_with(&job_prefix)),
                        // Partition vanished → that client holds nothing.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                        // Can't read a partition that exists → can't conclude
                        // "not leased elsewhere"; propagate instead of guessing.
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
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        remove_file_idempotent(self.leased_dir.join(leased_key(job_id, client_id, expiry))).await
    }

    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        let src = self.avail_dir.join(avail_filename(job_id, expires_at));
        let dst = self
            .leased_dir
            .join(leased_key(job_id, client_id, lease_expiry));
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
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        let src = self
            .leased_dir
            .join(leased_key(job_id, client_id, lease_expiry));
        let avail_dir = self.avail_dir.clone();
        let job_id = job_id.clone();
        run_blocking(move || {
            // Both the read and the rename treat a missing source as `Gone`:
            // the lease can vanish at either step when the holder's terminal
            // submission (or another recycler) beats us to it.
            let content = match std::fs::read_to_string(&src) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(RecycleResult::Gone);
                }
                Err(e) => return Err(e.into()),
            };
            let body: serde_json::Value = serde_json::from_str(&content)?;
            // `expires_at` is optional in a job body (`planner.md`): an absent or
            // null field means the job never auto-expires, so recycle it to
            // `avail/{job_id}.never.json`. Only a *present* value that isn't a
            // parseable timestamp/`never` is a corrupt body worth failing on.
            let expires_at: ExpiresAt = match body.get("expires_at") {
                None | Some(serde_json::Value::Null) => ExpiresAt::Never,
                Some(v) => v
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("`expires_at` in job body is not a string"))?
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid expires_at in job body: {e}"))?,
            };
            // Ensure `avail/` exists before renaming into it: `rename` reports
            // a missing destination dir and a missing source with the same
            // `NotFound`, so without this a lost `avail/` would masquerade as
            // `Gone` and silently strand every lease in `leased/`.
            std::fs::create_dir_all(&avail_dir)?;
            match std::fs::rename(&src, avail_dir.join(avail_filename(&job_id, expires_at))) {
                Ok(()) => Ok(RecycleResult::Recycled),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RecycleResult::Gone),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn write_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        let path = self.denied_dir.join(format!("{}.{}", job_id, client_id));
        run_blocking(move || Ok(std::fs::write(&path, b"")?)).await
    }

    async fn list_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<Vec<ClientId>> {
        let denied_dir = self.denied_dir.clone();
        let prefix = format!("{}.", job_id);
        run_blocking(move || {
            let mut clients = Vec::new();
            for entry in std::fs::read_dir(&denied_dir)? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if let Some(rest) = name.strip_prefix(&prefix) {
                    clients.push(ClientId::try_new(rest)?);
                }
            }
            Ok(clients)
        })
        .await
    }

    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let denied_dir = self.denied_dir.clone();
        let prefix = format!("{}.", job_id);
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
        remove_file_idempotent(self.denied_dir.join(format!("{}.{}", job_id, client_id))).await
    }

    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        let denied_dir = self.denied_dir.clone();
        run_blocking(move || {
            let entries = match std::fs::read_dir(&denied_dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(e.into()),
            };
            // Drop entries that fail to read, and **log + skip** malformed
            // markers: a bad marker is an anomaly to surface, not a fatal error,
            // and leaving it un-GC'd is harmless (the GC sweep only ever
            // *deletes* markers for jobs gone from `avail/`).
            let markers = entries
                .filter_map(|e| e.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
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
            // Tolerate concurrent mutation (e.g. `clients delete` removing a
            // client's markers mid-sweep): skip dir entries that error and
            // partitions that vanished rather than failing the whole sweep.
            // Malformed names (a bad client-id partition or an unparseable marker)
            // are **logged and skipped** rather than fatal — an anomaly to surface,
            // and leaving one un-GC'd is harmless (the sweep only ever deletes).
            let mut result = Vec::new();
            for client_entry in client_entries.filter_map(|e| e.ok()) {
                if !client_entry
                    .file_type()
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let dir_name = client_entry.file_name().to_string_lossy().into_owned();
                let client_id = match ClientId::try_new(dir_name.as_str()) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(partition = %dir_name, error = %e, "skipping eligible partition with invalid client_id");
                        continue;
                    }
                };
                let job_entries = match std::fs::read_dir(client_entry.path()) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                for job_entry in job_entries.filter_map(|e| e.ok()) {
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
        let path = self
            .pending_reindex_dir
            .join(crate::todo_filename::pending_reindex_filename(client_id));
        run_blocking(move || Ok(std::fs::write(&path, b"")?)).await
    }

    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        let pending_reindex_dir = self.pending_reindex_dir.clone();
        run_blocking(move || {
            // Entry read errors propagate (the gate must not lose a flag
            // silently); an unparseable *name* is foreign cruft — warned and
            // skipped, per the trait contract.
            std::fs::read_dir(&pending_reindex_dir)?
                .map(|e| anyhow::Ok(e?.file_name().to_string_lossy().into_owned()))
                .filter_map(|r| {
                    r.map(|name| {
                        match crate::todo_filename::parse_pending_reindex_filename(&name) {
                            Ok(client_id) => Some((client_id, name)),
                            Err(_) => {
                                tracing::warn!(
                                    key = %name,
                                    "skipping unparseable pending-reindex flag"
                                );
                                None
                            }
                        }
                    })
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
                            crate::todo_filename::parse_pending_reindex_filename(&name),
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
            // Entry read errors propagate (a lost flag is lost reindex debt);
            // an unparseable *name* is foreign cruft, not a flag — every
            // system-written flag is a valid `JobId` by construction — so it
            // is warned and skipped rather than wedging every maintenance
            // run until an operator deletes the file.
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

    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let path = self.tmp_dir.join(tmp_filename(job_id));
        let bytes = serde_json::to_vec(body)?;
        run_blocking(move || Ok(std::fs::write(&path, bytes)?)).await
    }

    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        // POSIX `rename` within one filesystem is atomic, so the job appears in
        // `avail/` whole or not at all — the same guarantee `claim_job` relies
        // on. A missing `tmp/` source surfaces as an error (the caller staged it
        // immediately before), not a silent no-op. Both a missing source and a
        // missing `avail/` dir surface as a bare `NotFound`, so the paths are
        // named in the error to keep the two distinguishable when debugging.
        let src = self.tmp_dir.join(tmp_filename(job_id));
        let dst = self.avail_dir.join(avail_filename(job_id, expires_at));
        run_blocking(move || {
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

    async fn read_eligible_cursor(&self) -> anyhow::Result<Option<String>> {
        let cursor_path = self.cursor_path.clone();
        run_blocking(move || match std::fs::read_to_string(&cursor_path) {
            Ok(s) => Ok(Some(s.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn write_eligible_cursor(&self, key: &str) -> anyhow::Result<()> {
        let cursor_path = self.cursor_path.clone();
        let key = key.to_owned();
        run_blocking(move || Ok(std::fs::write(&cursor_path, key.as_bytes())?)).await
    }

    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        let path = self.gc_candidates_path.clone();
        run_blocking(move || match std::fs::read(&path) {
            Ok(bytes) => Ok(crate::stores::parse_gc_candidates(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
        let path = self.gc_candidates_path.clone();
        let bytes = serde_json::to_vec(candidates)?;
        run_blocking(move || Ok(std::fs::write(&path, &bytes)?)).await
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

    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        let suspended_dir = self.suspended_dir.clone();
        run_blocking(move || {
            // A malformed marker — an entry that fails to read, a filename with
            // an invalid client id, or an unreadable/unparseable JSON body — is
            // logged and skipped, not fatal: one bad file must not hide every
            // other suspended client from the operator. Mirrors the leniency of
            // `list_all_eligible` / `list_pending_reindex`.
            let mut result = Vec::new();
            for entry in std::fs::read_dir(&suspended_dir)?.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id_str) = name.strip_suffix(".json") else {
                    continue;
                };
                let client_id = match ClientId::try_new(id_str) {
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

    /// Always atomic: `claim_job` / `renew_lease` use `std::fs::rename`, which is
    /// atomic on POSIX within a filesystem. Nothing to probe.
    async fn validate_backend(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::TEST_LIST_LIMIT;
    use crate::stores::local_fs::*;
    use crate::types::{BenchmarkId, ClientId, JobId};
    use anyhow::Context;
    use chrono::Utc;
    use rstest::rstest;

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
        // Flat processed/ — no bucketing.
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
        // Lands under submissions/score-queue/to_do/, isolated from to_finalize.
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

        // In to_do: find_job reports Scoring with the bare submission body.
        store.enqueue(ScoreQueueStage::ToDo, &job_id, &body).await?;
        let found = store.find_job(&job_id).await?.context("expected to_do")?;
        assert_eq!(found.state, JobState::Scoring);
        assert_eq!(found.body, body);

        // Advance to to_finalize, where the payload wraps the submission
        // alongside its score; find_job unwraps it back to the bare body.
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

        // Once drained from the queue, it is gone again.
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

        // Just-written objects are younger than an hour → all kept.
        let summary = store
            .prune_unverified(std::time::Duration::from_secs(3600), false)
            .await?;
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.kept, 2);

        // Dry-run with a zero threshold reports both as prunable but
        // removes nothing.
        let dry = store
            .prune_unverified(std::time::Duration::ZERO, true)
            .await?;
        assert_eq!(dry.deleted, 2);
        assert_eq!(store.list_unverified_client(&client).await?.len(), 2);

        // Live zero-threshold prune removes everything.
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

    /// `make_metric_row` with an explicit `scored_at` — re-score tests need
    /// the newer copy to carry a later `scored_at`, as production does.
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
            value,
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
        assert_eq!(job_metrics.scored_at, "2025-03-10T12:02:00+00:00");

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_warehouse_read_keeps_latest_on_rescore() -> anyhow::Result<()> {
        // The write path appends without dedup (a crash-retry re-score adds a
        // second copy with a later scored_at), and the read returns only the
        // newest scoring run — not duplicate rows.
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
        // Re-score: same job_id/result_ids, new values, later scored_at.
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
        // A re-score that produces *fewer* metrics must not leave stale rows
        // from the older copy: the whole older run is dropped by scored_at.
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

        // Copy-1: two metrics.
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
        // Copy-2 (newer): only one metric.
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

        // A submission scored today lands in a `day=` partition and is
        // found within the window.
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

        // A legacy `month=` partition far in the past is outside the window.
        // The hard cap means it is not returned — there is no full-scan
        // fallback — even though the rows exist on disk.
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

        let store = LocalFsCatalogStore::new(benchmarks_dir);
        let catalog = store.load_catalog().await?;
        assert!(catalog.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_auth_store_client_crud() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let clients_dir = dir.path().join("clients");
        std::fs::create_dir_all(&clients_dir)?;

        let store = LocalFsAuthStore::new(clients_dir);

        let public_key = crate::validated::PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let client = Client {
            client_id: client::derive_client_id(&public_key)?,
            public_key: public_key.clone(),
            organization: crate::validated::NonEmptyTrimmedString::try_new("test-org")?,
            client_details: crate::validated::NonEmptyTrimmedString::try_new("details")?,
            contact_email: crate::validated::ContactEmail::try_new("test@example.com")?,
            status: crate::client::ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };

        store.put_client(&client).await?;
        let loaded = store
            .get_client(&client.client_id)
            .await?
            .context("expected client")?;
        assert_eq!(loaded.client_id, client.client_id);
        assert_eq!(store.list_clients().await?.len(), 1);
        assert!(store.has_public_key(&public_key).await?);

        store.delete_client(&client.client_id).await?;
        assert!(store.get_client(&client.client_id).await?.is_none());

        Ok(())
    }

    /// Here the first write wins by `hard_link` refusing an occupied path.
    #[tokio::test]
    async fn test_localfs_signature_migration_keeps_the_first_sighting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsAuthStore::new(dir.path().join("clients"));
        crate::stores::assert_signature_migration_keeps_first_sighting(&store).await
    }

    /// A marker whose contents cannot be read still withdraws the fallback.
    /// Existence alone carries the decision the auth path depends on, so an
    /// unparsable marker has to fail toward refusing the client rather than
    /// toward handing back a fallback it has already proven it does not need.
    /// The operator listing takes the other reading — an entry it cannot parse
    /// is one it cannot report a date for — so the two disagree by design.
    #[tokio::test]
    async fn test_localfs_an_unreadable_marker_still_reads_as_migrated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsAuthStore::new(dir.path().join("clients"));
        let client_id = ClientId::try_new("ev1_unreadable")?;

        // The marker tree is a sibling of `clients/`, not a child of it.
        let markers = dir.path().join("signature-migration");
        std::fs::create_dir_all(&markers)?;
        std::fs::write(markers.join(format!("{client_id}.json")), b"")?;

        assert!(store.has_signature_migration(&client_id).await?);
        assert!(store.list_signature_migrations().await?.is_empty());
        Ok(())
    }

    /// Staging is invisible to the operator listing. A write interrupted
    /// between staging the record and linking it into place leaves the staged
    /// file behind; it names no client the listing will select, so it neither
    /// appears as a migration nor blocks the client's next attempt from
    /// recording one.
    #[tokio::test]
    async fn test_localfs_a_stranded_staged_record_is_not_a_migration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsAuthStore::new(dir.path().join("clients"));
        let client_id = ClientId::try_new("ev1_interrupted")?;
        let at = Utc::now();

        let markers = dir.path().join("signature-migration");
        std::fs::create_dir_all(&markers)?;
        std::fs::write(
            markers.join(format!(
                "{client_id}.00000000-0000-4000-8000-000000000000.staged"
            )),
            serde_json::to_vec(&crate::client::SignatureMigration { first_seen: at })?,
        )?;

        assert!(!store.has_signature_migration(&client_id).await?);
        assert!(store.list_signature_migrations().await?.is_empty());
        assert_eq!(
            store.record_signature_migration(&client_id, at).await?,
            crate::stores::MigrationRecord::First
        );
        assert_eq!(store.list_signature_migrations().await?.len(), 1);
        Ok(())
    }

    /// An empty tree is "nobody has migrated", not an error — the listing runs
    /// against a store where no client has ever sent a `v1` signature.
    #[tokio::test]
    async fn test_localfs_signature_migrations_absent_tree_is_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsAuthStore::new(dir.path().join("clients"));
        assert!(store.list_signature_migrations().await?.is_empty());
        Ok(())
    }

    fn auth_test_client(id: &str, pk_suffix: &str, org: &str) -> anyhow::Result<Client> {
        // 64-char hex pubkey with a distinct 2-char suffix so clients differ.
        let public_key = crate::validated::PublicKeyHex::try_new(format!(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567{pk_suffix}"
        ))?;
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key,
            organization: crate::validated::NonEmptyTrimmedString::try_new(org)?,
            client_details: crate::validated::NonEmptyTrimmedString::try_new("details")?,
            contact_email: crate::validated::ContactEmail::try_new("a@b.com")?,
            status: crate::client::ClientStatus::Approved,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        })
    }

    #[tokio::test]
    async fn test_localfs_auth_store_tag_markers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let clients_dir = dir.path().join("clients");
        std::fs::create_dir_all(&clients_dir)?;
        let store = LocalFsAuthStore::new(clients_dir);

        let a = auth_test_client("ev1_a", "00", "acme")?;
        let b = auth_test_client("ev1_b", "11", "acme")?;
        store.put_client(&a).await?;
        store.put_client(&b).await?;

        crate::stores::assert_tag_store_roundtrip(&store, &a.client_id, &b.client_id).await
    }

    /// Backend-specific guard: tagging a client never writes tags into its JSON
    /// record (they live only in the marker trees).
    #[tokio::test]
    async fn test_localfs_tags_not_in_record() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let clients_dir = dir.path().join("clients");
        std::fs::create_dir_all(&clients_dir)?;
        let store = LocalFsAuthStore::new(clients_dir.clone());

        let a = auth_test_client("ev1_a", "00", "acme")?;
        store.put_client(&a).await?;
        store
            .add_client_tag(
                &a.client_id,
                &crate::validated::Tag::try_new("team-mobile")?,
            )
            .await?;

        let raw = std::fs::read_to_string(clients_dir.join(format!("{}.json", a.client_id)))?;
        assert!(!raw.contains("tags"), "record must not contain tags");
        Ok(())
    }

    /// `reindex_tags` repairs every drift the non-atomic double-write can leave:
    /// a forward marker with no reverse, an orphan reverse, and markers for a
    /// since-deleted client. Converges the reverse tree to the forward truth,
    /// and is a no-op on a second run.
    #[tokio::test]
    async fn test_reindex_tags_repairs_drift() -> anyhow::Result<()> {
        use crate::validated::Tag;

        let dir = tempfile::tempdir()?;
        let clients_dir = dir.path().join("clients");
        std::fs::create_dir_all(&clients_dir)?;
        let store = LocalFsAuthStore::new(clients_dir);

        let a = auth_test_client("ev1_a", "00", "acme")?;
        store.put_client(&a).await?;
        let team = Tag::try_new("team-mobile")?;
        let east = Tag::try_new("us-east")?;
        store.add_client_tag(&a.client_id, &team).await?; // consistent in both trees

        let by_client = dir.path().join("tags-index/by-client");
        let by_tag = dir.path().join("tags-index/by-tag");
        let touch = |p: std::path::PathBuf| -> anyhow::Result<()> {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(p, [])?;
            Ok(())
        };

        // Drift 1: forward marker with no reverse (crash after forward write).
        touch(by_client.join(a.client_id.as_str()).join(east.as_str()))?;
        // Drift 2: orphan reverse (crash after forward delete, before reverse).
        touch(by_tag.join("ghost").join(a.client_id.as_str()))?;
        // Drift 3: both markers for a client whose record is gone.
        touch(by_client.join("ev1_ghostclient").join(team.as_str()))?;
        touch(by_tag.join(team.as_str()).join("ev1_ghostclient"))?;

        let report = crate::stores::reindex_tags(&store).await?;
        assert_eq!(report.added, 1, "east's reverse marker created");
        assert_eq!(report.removed, 2, "orphan reverse + deleted-client pair");

        // Trees now agree: a has {team, east}; ghosts gone.
        assert_eq!(
            store.get_client_tags(&a.client_id).await?,
            std::collections::BTreeSet::from([team.clone(), east.clone()])
        );
        assert_eq!(
            store.list_client_ids_by_tag(&east).await?,
            vec![a.client_id.clone()]
        );
        assert!(
            store
                .list_client_ids_by_tag(&Tag::try_new("ghost")?)
                .await?
                .is_empty()
        );
        assert!(
            store
                .get_client_tags(&ClientId::try_new("ev1_ghostclient")?)
                .await?
                .is_empty()
        );

        // Second run is a no-op.
        let again = crate::stores::reindex_tags(&store).await?;
        assert_eq!((again.added, again.removed), (0, 0));
        Ok(())
    }

    #[tokio::test]
    async fn test_build_local_fs_auth_store_uses_separate_dir() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let auth_dir = tempfile::tempdir()?;

        // Build stores from main data_dir
        let config = Config {
            evals_server_url: "http://unused".to_string(),
            http_timeout_secs: 10,
            storage: crate::config::StorageConfig::local_fs(data_dir.path().to_path_buf()),
            auth_storage: crate::config::StorageConfig::local_fs(data_dir.path().to_path_buf()),
            ..Config::default()
        };
        let mut stores = build_local_fs_stores(&config)?;

        // Override auth with separate dir
        let auth_storage = crate::config::StorageConfig::local_fs(auth_dir.path().to_path_buf());
        stores.auth = super::build_local_fs_auth_store(&auth_storage)?;

        // Write a client via the separate auth store
        let public_key = crate::validated::PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let c = Client {
            client_id: client::derive_client_id(&public_key)?,
            public_key: public_key.clone(),
            organization: crate::validated::NonEmptyTrimmedString::try_new("test")?,
            client_details: crate::validated::NonEmptyTrimmedString::try_new("d")?,
            contact_email: crate::validated::ContactEmail::try_new("t@t.com")?,
            status: crate::client::ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };
        stores.auth.put_client(&c).await?;

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

        // Auth store can read it back
        let loaded = stores.auth.get_client(&c.client_id).await?.unwrap();
        assert_eq!(loaded.client_id, c.client_id);

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
        // Regression guard: a successful write must disarm the
        // `TmpFileGuard` *and* the rename must remove the tmp name
        // from the dir. Asserts the flat eval_sample_results dir
        // contains exactly the final `.parquet` afterward.
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
        }];
        store.write(&JobId::new_unchecked("job-a"), &rows).await?;
        store.write(&JobId::new_unchecked("job-b"), &rows).await?;

        // Junk that must be skipped: an interrupted-write tmp file, a
        // non-parquet file, and a parquet whose stem isn't a valid job id.
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
        }];

        store.write(&JobId::new_unchecked("job1"), &rows_v1).await?;

        // Overwrite with corrected result
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

    use crate::stores::{
        ClaimResult, RecycleResult, RenewLeaseResult, SuspensionRecord, TodoStore,
    };
    use crate::todo_filename::{avail_filename, leased_key};
    use crate::types::ExpiresAt;
    use chrono::DateTime;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    /// Plant a `leased/` fixture file directly, creating the `{client_id}/`
    /// partition dir first (the partitioned layout requires it to exist).
    fn plant_leased(
        leased_dir: &std::path::Path,
        job: &JobId,
        client: &ClientId,
        expiry: DateTime<Utc>,
        body: &[u8],
    ) -> anyhow::Result<()> {
        let path = leased_dir.join(leased_key(job, client, expiry));
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, body)?;
        Ok(())
    }

    /// Build a `LocalFsTodoStore` over a fresh tempdir with the same `todo/`
    /// subdirectory layout `build_local_fs_stores` creates. Returns the store,
    /// the `TempDir` guard (kept alive by the caller), and the `todo/` root so
    /// tests can plant fixture files directly.
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
            "tmp",
            "suspended",
        ]
        .into_iter()
        .try_for_each(|sub| std::fs::create_dir_all(todo_dir.join(sub)))?;
        let store = LocalFsTodoStore::new(todo_dir.clone());
        Ok((store, dir, todo_dir))
    }

    fn dt(s: &str) -> anyhow::Result<DateTime<Utc>> {
        Ok(s.parse::<DateTime<Utc>>()?)
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
        assert!(store.list_avail(None, TEST_LIST_LIMIT).await?.is_empty());

        store.promote_avail(&job, exp).await?;
        // Promotion moves it under the expires_at-encoded avail name and clears
        // the tmp slot — a partial (tmp-only) write never appears in avail/.
        assert!(!todo_dir.join("tmp").join("job1.json").exists());
        assert_eq!(
            store.list_avail(None, TEST_LIST_LIMIT).await?,
            vec![avail_filename(&job, exp)]
        );
        let got = store
            .get_avail(&job, exp)
            .await?
            .context("expected avail body")?;
        assert_eq!(got["job_id"], "job1");
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_plan_store_roundtrip_list_delete() -> anyhow::Result<()> {
        use crate::plan::{PlanStatus, sample_manifest};
        use crate::types::PlanId;

        let dir = tempfile::tempdir()?;
        let store =
            LocalFsPlanStore::new(dir.path().join("plans"), dir.path().join("cancelled_plans"));

        let active_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let done_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        // Round-trips with a progress_snapshot and without one.
        let active = sample_manifest(active_id.clone(), PlanStatus::Active, true);
        let done = sample_manifest(done_id.clone(), PlanStatus::Complete, false);
        store.put_plan(&active).await?;
        store.put_plan(&done).await?;

        assert_eq!(store.get_plan(&active_id).await?.as_ref(), Some(&active));
        assert_eq!(store.get_plan(&done_id).await?.as_ref(), Some(&done));

        // `None` returns all; a status filter narrows it.
        let mut all = store.list_plans(None).await?;
        all.sort_by(|a, b| a.plan_id.as_str().cmp(b.plan_id.as_str()));
        assert_eq!(all, vec![active.clone(), done.clone()]);
        assert_eq!(
            store.list_plans(Some(PlanStatus::Active)).await?,
            vec![active]
        );
        assert!(
            store
                .list_plans(Some(PlanStatus::Cancelled))
                .await?
                .is_empty()
        );

        // delete removes it and is idempotent.
        store.delete_plan(&done_id).await?;
        assert!(store.get_plan(&done_id).await?.is_none());
        store.delete_plan(&done_id).await?; // no-op, no error
        assert_eq!(store.list_plans(None).await?.len(), 1);

        // A never-created plan reads back as None.
        let absent = PlanId::from_uuid(uuid::Uuid::from_u128(99));
        assert!(store.get_plan(&absent).await?.is_none());
        Ok(())
    }

    /// Cancel markers round-trip, are idempotent in both directions, and live in
    /// a keyspace `list_plans` cannot see.
    #[tokio::test]
    async fn test_localfs_cancel_marker_roundtrip() -> anyhow::Result<()> {
        use crate::plan::{PlanStatus, sample_manifest};
        use crate::types::PlanId;

        let dir = tempfile::tempdir()?;
        let store =
            LocalFsPlanStore::new(dir.path().join("plans"), dir.path().join("cancelled_plans"));

        let a = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let b = PlanId::from_uuid(uuid::Uuid::from_u128(2));

        // Before any cancel: no marker, and listing an absent directory is empty
        // rather than an error.
        assert!(!store.has_cancel_marker(&a).await?);
        assert!(store.list_cancel_markers().await?.is_empty());

        store.write_cancel_marker(&a).await?;
        assert!(store.has_cancel_marker(&a).await?);
        assert!(!store.has_cancel_marker(&b).await?, "marker is per-plan");
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        // Re-cancelling is a no-op, not an error or a duplicate entry — the
        // marker is a request, not a compare-and-swap.
        store.write_cancel_marker(&a).await?;
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        // A marker does not pollute the manifest listing: put a manifest for the
        // same plan and `list_plans` still sees exactly one object.
        store
            .put_plan(&sample_manifest(a.clone(), PlanStatus::Active, false))
            .await?;
        assert_eq!(store.list_plans(None).await?.len(), 1);

        // Deleting the marker leaves the manifest alone, and is idempotent.
        store.delete_cancel_marker(&a).await?;
        assert!(!store.has_cancel_marker(&a).await?);
        store.delete_cancel_marker(&a).await?; // no-op, no error
        assert!(store.get_plan(&a).await?.is_some(), "manifest survives");
        Ok(())
    }

    /// A foreign file in the marker directory is warned about and skipped, not an
    /// error — the same fail-soft-on-cruft contract as `list_pending_reindex`.
    #[tokio::test]
    async fn test_localfs_cancel_marker_skips_unparseable() -> anyhow::Result<()> {
        use crate::types::PlanId;

        let dir = tempfile::tempdir()?;
        let cancelled = dir.path().join("cancelled_plans");
        let store = LocalFsPlanStore::new(dir.path().join("plans"), cancelled.clone());

        let good = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        store.write_cancel_marker(&good).await?;
        // `/` and `.` are outside the PlanId charset, so a dotted name cannot be
        // a plan id.
        std::fs::write(cancelled.join("not.a.plan.id"), b"")?;

        assert_eq!(store.list_cancel_markers().await?, vec![good]);
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
        std::fs::write(
            todo_dir.join("avail").join(avail_filename(&job, exp)),
            serde_json::to_vec(&body)?,
        )?;

        match store.claim_job(&job, exp, &client, lease).await? {
            ClaimResult::Claimed(got) => assert_eq!(got["job_id"], "job1"),
            ClaimResult::Gone => anyhow::bail!("expected Claimed, got Gone"),
        }

        // avail/ entry moved to leased/.
        assert!(store.list_avail(None, TEST_LIST_LIMIT).await?.is_empty());
        assert_eq!(
            store.list_leased().await?,
            vec![leased_key(&job, &client, lease)]
        );
        // The targeted per-client lookup finds the same single lease.
        assert_eq!(
            store.list_leased_for_client(&client).await?,
            vec![leased_key(&job, &client, lease)]
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

        // Same job under two different expiries — the caller (submission
        // teardown) knows the job_id but not which expiry is on disk.
        for exp in [ExpiresAt::Never, ExpiresAt::At(dt("2026-06-15T08:30:00Z")?)] {
            std::fs::write(avail.join(avail_filename(&job, exp)), b"{}")?;
        }
        // A different job must survive the by-job delete.
        std::fs::write(avail.join(avail_filename(&other, ExpiresAt::Never)), b"{}")?;

        store.delete_avail_by_job(&job).await?;

        assert_eq!(
            store.list_avail(None, TEST_LIST_LIMIT).await?,
            vec![avail_filename(&other, ExpiresAt::Never)],
            "only the targeted job's entries (all expiries) are removed"
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
        plant_leased(&leased, &job_r, &client, old, b"{}")?;
        assert_eq!(
            store.renew_lease(&job_r, &client, new).await?,
            RenewLeaseResult::Renewed
        );
        assert!(leased.join(leased_key(&job_r, &client, new)).exists());

        // WrongClient: a lease for the same job is held by a different client
        // (in that client's partition).
        let job_w = JobId::new_unchecked("jobwrong");
        let other = ClientId::try_new("client2")?;
        plant_leased(&leased, &job_w, &other, old, b"{}")?;
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

    #[tokio::test]
    async fn test_localfs_list_leased_tolerates_stray_entries() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let leased = todo_dir.join("leased");
        let client = ClientId::try_new("client1")?;
        let job = JobId::new_unchecked("job1");
        let lease = dt("2026-06-15T08:30:00Z")?;
        plant_leased(&leased, &job, &client, lease, b"{}")?;

        // A stray non-dir file at the partition root and an empty client
        // partition must be skipped, not error the listing (mirrors the
        // race where a partition is created/emptied concurrently).
        std::fs::write(leased.join(".DS_Store"), b"junk")?;
        std::fs::create_dir_all(leased.join("emptyclient"))?;

        assert_eq!(
            store.list_leased().await?,
            vec![leased_key(&job, &client, lease)]
        );
        Ok(())
    }

    /// Recycling a live lease renames it `leased/ → avail/`, deriving the avail
    /// filename's `expires_at` from the job body: a valid `expires_at` is
    /// preserved; an absent one defaults to `never` (`planner.md` makes the
    /// field optional). A missing `avail/` directory is recreated rather than
    /// stranding the lease — `rename` reports it as `NotFound`, which is
    /// otherwise indistinguishable from `Gone`. The body round-trips into
    /// `avail/` in every case.
    #[rstest]
    #[case::preserves_expires_at(Some("2026-07-01T00:00:00Z"), false)]
    #[case::absent_expires_at_defaults_never(None, false)]
    #[case::recreates_missing_avail_dir(None, true)]
    #[tokio::test]
    async fn test_localfs_todo_recycle_lease_recycles(
        #[case] body_expires_at: Option<&str>,
        #[case] remove_avail_dir: bool,
    ) -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("jobx");
        let client = ClientId::try_new("client1")?;
        let lease = dt("2026-06-20T10:00:00Z")?;

        // The job's own expiry lives only in the body (the leased filename
        // encodes the lease expiry, not the job expiry) and drives the avail
        // rename target.
        let expected = match body_expires_at {
            Some(s) => ExpiresAt::At(dt(s)?),
            None => ExpiresAt::Never,
        };
        let mut body = serde_json::json!({ "job_id": "jobx" });
        if body_expires_at.is_some() {
            body["expires_at"] = serde_json::json!(expected.to_string());
        }
        plant_leased(
            &todo_dir.join("leased"),
            &job,
            &client,
            lease,
            &serde_json::to_vec(&body)?,
        )?;
        if remove_avail_dir {
            std::fs::remove_dir_all(todo_dir.join("avail"))?;
        }

        assert_eq!(
            store.recycle_lease(&job, &client, lease).await?,
            RecycleResult::Recycled
        );

        assert!(store.list_leased().await?.is_empty());
        assert_eq!(
            store.list_avail(None, TEST_LIST_LIMIT).await?,
            vec![avail_filename(&job, expected)]
        );
        let recycled = store
            .get_avail(&job, expected)
            .await?
            .context("avail body")?;
        assert_eq!(recycled["job_id"], "jobx");

        Ok(())
    }

    /// A lease that no longer exists (already recycled or torn down by a
    /// terminal submission) is `Gone`, not an error.
    #[tokio::test]
    async fn test_localfs_todo_recycle_lease_gone_when_absent() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("jobgone");
        let client = ClientId::try_new("client1")?;
        let lease = dt("2026-06-20T10:00:00Z")?;

        assert_eq!(
            store.recycle_lease(&job, &client, lease).await?,
            RecycleResult::Gone
        );
        Ok(())
    }

    /// A *present* but non-string / unparseable `expires_at` is a corrupt body,
    /// distinct from an absent one, and still errors.
    #[tokio::test]
    async fn test_localfs_todo_recycle_lease_errors_on_malformed_expires_at() -> anyhow::Result<()>
    {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("jobz");
        let client = ClientId::try_new("client1")?;
        let lease = dt("2026-06-20T10:00:00Z")?;

        let body = serde_json::json!({ "job_id": "jobz", "expires_at": "not-a-timestamp" });
        plant_leased(
            &todo_dir.join("leased"),
            &job,
            &client,
            lease,
            &serde_json::to_vec(&body)?,
        )?;

        assert!(store.recycle_lease(&job, &client, lease).await.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_list_avail_paginates_and_ignores_non_json() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let avail = todo_dir.join("avail");
        ["a", "b", "c"].into_iter().try_for_each(|id| {
            std::fs::write(
                avail.join(avail_filename(&JobId::new_unchecked(id), ExpiresAt::Never)),
                b"{}",
            )
        })?;
        // A non-`.json` sibling must be skipped.
        std::fs::write(avail.join("notjson.txt"), b"")?;

        let two = NonZeroUsize::new(2).context("nonzero")?;
        assert_eq!(
            store.list_avail(None, two).await?,
            vec!["a.never.json".to_string(), "b.never.json".to_string()]
        );
        // start_after is exclusive.
        assert_eq!(
            store.list_avail(Some("b.never.json"), two).await?,
            vec!["c.never.json".to_string()]
        );
        assert_eq!(
            store.list_avail(None, TEST_LIST_LIMIT).await?,
            vec![
                "a.never.json".to_string(),
                "b.never.json".to_string(),
                "c.never.json".to_string()
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_get_avail_by_job() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;
        let job = JobId::new_unchecked("job1");

        // Absent → None (no avail entry for the job).
        assert!(store.get_avail_by_job(&job).await?.is_none());

        // Present under an arbitrary expiry → body returned without the caller
        // needing to know the expires_at encoded in the filename.
        let body = serde_json::json!({ "job_id": "job1", "expires_at": "never" });
        std::fs::write(
            todo_dir
                .join("avail")
                .join(avail_filename(&job, ExpiresAt::Never)),
            serde_json::to_vec(&body)?,
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
        store.delete_lease(&job, &client, when).await?;
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

    /// A malformed `suspended/*.json` (truncated write, hand-edit) must not
    /// sink the whole listing and hide every other suspended client from the
    /// operator — the bad file is skipped, the good ones are returned.
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

        store.write_denied(&job, &ca).await?;
        store.write_denied(&job, &cb).await?;

        let mut denied = store.list_denied_for_job(&job).await?;
        denied.sort();
        assert_eq!(denied, vec![ca.clone(), cb.clone()]);

        // `list_all_denied` spans jobs and must split `{job_id}.{client_id}` on
        // the `.`, so a client id that itself contains `_` round-trips.
        let job2 = JobId::new_unchecked("jobe");
        let underscored = ClientId::try_new("ev1_abc")?;
        store.write_denied(&job2, &underscored).await?;

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
        assert!(store.list_denied_for_job(&job).await?.is_empty());
        // The other job's marker is untouched.
        assert_eq!(store.list_all_denied().await?, vec![(job2, underscored)]);

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_pending_reindex_round_trip() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;
        // `_`-bearing id: the flag-key nonce is split off on the `.`
        // separator, which lies outside the id charset, so a client id
        // containing `_` must round-trip.
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
        // Foreign cruft (`.` is outside the JobId charset) is skipped, not an
        // error — one stray file must not wedge the listing.
        std::fs::write(todo_dir.join("pending-reindex-jobs").join(".DS_Store"), b"")?;

        assert!(store.list_pending_reindex_jobs().await?.is_empty());
        store.write_pending_reindex_job(&job).await?;
        assert_eq!(store.list_pending_reindex_jobs().await?, vec![job.clone()]);
        store.delete_pending_reindex_job(&job).await?;
        assert!(store.list_pending_reindex_jobs().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_todo_eligible_cursor_round_trip() -> anyhow::Result<()> {
        let (store, _dir, _todo_dir) = todo_store()?;

        assert!(store.read_eligible_cursor().await?.is_none());
        store.write_eligible_cursor("b.never.json").await?;
        assert_eq!(
            store.read_eligible_cursor().await?,
            Some("b.never.json".to_string())
        );

        Ok(())
    }

    /// Missing state reads as empty, a written set round-trips, and corrupt
    /// state also reads as empty (the safe direction — see
    /// [`crate::stores::parse_gc_candidates`]).
    #[tokio::test]
    async fn test_localfs_todo_gc_candidates_round_trip() -> anyhow::Result<()> {
        let (store, _dir, todo_dir) = todo_store()?;

        assert!(store.read_gc_candidates().await?.is_empty());

        let candidates = std::collections::HashSet::from([
            "eligible/clients/client-a/job-a.never".to_string(),
            "denied/job-b.client-b".to_string(),
        ]);
        store.write_gc_candidates(&candidates).await?;
        assert_eq!(store.read_gc_candidates().await?, candidates);

        std::fs::write(todo_dir.join(".gc-candidates"), b"not json")?;
        assert!(store.read_gc_candidates().await?.is_empty());

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
}
