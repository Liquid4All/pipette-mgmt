//! The handles and process-local caches every route reads.

use std::sync::Arc;

use anyhow::Context;

use pipette_core::config::Config;
use pipette_pipeline::catalog_cache::CatalogCache;
use pipette_pipeline::hf_import::HfImports;
use pipette_pipeline::plan_cells::CellCache;
use pipette_pipeline::plan_results::ResultsDb;
use pipette_store::{
    AuditStore, CampaignStore, CatalogImportRunStore, CatalogStore, ClientStore, DatasetCacheStore,
    DescriptorCatalogStore, EvalSampleResultStore, KeyIndexStore, ModelParamsStore, OrgStore,
    PlanStore, Stores, SubmissionStore, TagStore, TodoStore, UserStore, VaultStore, WarehouseStore,
};

use crate::api_schema::openapi::OpenApi;
use crate::api_v1::plans::results_cache::PlanResultsCache;
use crate::auth::{LegacyUserKeys, LivenessThrottle, MigratedClients, ReplayCache};
use crate::clerk::ClerkSignIn;
use crate::extract::{HELD_SUBMISSION_SHARES, SUBMISSION_SHARES, SubmissionReaders};
use crate::registration_throttle::RegistrationThrottle;

/// Everything a handler reaches for that outlives one request. Every field is
/// `pub`, since the integration tests arrange and assert through them.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog_cache: Arc<CatalogCache>,
    /// Read directly for the per-organization catalogs, which are not cached,
    /// so a write is served on the next read.
    pub catalog_store: Arc<dyn CatalogStore>,
    /// Read on every request so host CLI changes are visible immediately.
    pub descriptor_catalog_store: Option<Arc<dyn DescriptorCatalogStore>>,
    /// The SQL history of importer runs. The server reads it on every request
    /// for the catalog status.
    pub catalog_import_runs: Option<Arc<dyn CatalogImportRunStore>>,
    pub http_client: reqwest::Client,
    /// pipette-duckdb, which a plan's results are read from; `None` without a
    /// `[duckdb]` section.
    pub results_db: Option<ResultsDb>,
    /// Each plan's cells, kept for the next read of its jobs. Process-local.
    pub cell_cache: Arc<CellCache>,
    /// The results of `complete` plans, kept for the next read of their jobs.
    /// Process-local.
    pub plan_results_cache: Arc<PlanResultsCache>,
    /// Spent signatures. Process-local: a multi-instance deployment protects
    /// each instance, not the fleet.
    pub replay_cache: Arc<ReplayCache>,
    /// Clients known to have migrated to `v1` signatures
    /// ([`crate::auth::MigratedClients`]).
    pub migrated_clients: Arc<MigratedClients>,
    /// The user keys this process has seen sign `v2`.
    pub legacy_user_keys: Arc<LegacyUserKeys>,
    /// When this process last stamped each client's liveness marker.
    pub liveness_throttle: Arc<LivenessThrottle>,
    /// The registrations this process admitted without a pre-auth key.
    pub registration_throttle: Arc<RegistrationThrottle>,
    /// The `[clerk]` sign-in, or `None` when the console signs in with keys alone.
    pub clerk: Option<Arc<ClerkSignIn>>,
    pub client_store: Arc<dyn ClientStore>,
    /// `users/` accounts and their keys, a keyspace disjoint from
    /// [`Self::client_store`]'s.
    pub user_store: Arc<dyn UserStore>,
    /// `orgs/`. Nothing on the signing path reads it: an organization
    /// authenticates nothing.
    pub org_store: Arc<dyn OrgStore>,
    /// `tags-index/` — the client tag index, over the `[storage]` root.
    pub tag_store: Arc<dyn TagStore>,
    /// `public-key-index/`: one keypair to one principal. Both registration
    /// paths write it, hence its own store.
    pub key_index_store: Arc<dyn KeyIndexStore>,
    pub submission_store: Arc<dyn SubmissionStore>,
    pub warehouse_store: Arc<dyn WarehouseStore>,
    pub eval_sample_result_store: Arc<dyn EvalSampleResultStore>,
    /// `todo/` job-queue tree, backed by `config.todo_storage()`.
    pub todo_store: Arc<dyn TodoStore>,
    /// `plans/` manifest tree.
    pub plan_store: Arc<dyn PlanStore>,
    /// `campaigns/`, written only by the `/api/v1/campaigns` routes; no cron
    /// pass touches it.
    pub campaign_store: Arc<dyn CampaignStore>,
    /// `model-params/`: the model parameter table, written by `/api/v1/model-params`
    /// and read for a filing's coverage warning and a result row's empty counts.
    pub model_params_store: Arc<dyn ModelParamsStore>,
    /// The audit log, written by `crate::audit` for each write request; `None`
    /// without a `[database]`, which alone keeps one.
    pub audit_store: Option<Arc<dyn AuditStore>>,
    /// The audit writes that run after their answer went out. `serve` waits for
    /// them before the process exits.
    pub audit_writes: tokio_util::task::TaskTracker,
    /// `dataset-cache/artifacts/`: repacked corpora and their probe verdicts,
    /// read by `GET /datasets/{benchmark_id}`.
    pub dataset_cache_store: Arc<dyn DatasetCacheStore>,
    /// `vault/` records and the `vault-resources/` blobs beside them: the
    /// private artifacts mgmt serves to one principal
    /// (`docs/design/vault-resources.md`).
    pub vault_store: Arc<dyn VaultStore>,
    /// One permit per `PUT /api/v1/vault/blobs` in flight: [`VAULT_UPLOADS_MAX`].
    pub vault_uploads: Arc<tokio::sync::Semaphore>,
    /// Shares of `submission_body_limit_bytes` reserved by submission bodies
    /// being read or parsed: [`SUBMISSION_SHARES`].
    pub submission_bodies: Arc<tokio::sync::Semaphore>,
    /// Shares a held submission body reserves on top of its
    /// [`Self::submission_bodies`] ones: [`HELD_SUBMISSION_SHARES`].
    pub held_submission_bodies: Arc<tokio::sync::Semaphore>,
    /// Shares held by parsed submission trees until their handler answers:
    /// [`SUBMISSION_SHARES`].
    pub submissions_in_flight: Arc<tokio::sync::Semaphore>,
    pub submission_readers: Arc<SubmissionReaders>,
    /// The HuggingFace import ingestion runs for a job body with a token. Its
    /// permits bound the imports and HuggingFace requests of the whole process.
    pub hf_imports: HfImports,
    /// The operator API's description, which `GET /api/v1/openapi.json` serves.
    pub openapi: Arc<OpenApi>,
}

/// Uploads streaming at once. The object store buffers up to ~190 MiB of
/// multipart parts per upload, so four stay under 800 MiB.
pub const VAULT_UPLOADS_MAX: usize = 4;

impl AppState {
    /// Build application state and its process-local caches from the stores. It
    /// loads the catalog itself, so every caller gets the same warm cache.
    pub async fn new(
        config: Config,
        stores: Stores,
        http_client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let hf_imports = HfImports::from_config(&config)
            .context("failed to build the HuggingFace import client")?;
        let openapi = Arc::new(OpenApi::build().context("failed to build the OpenAPI document")?);
        let catalog = stores.catalog.load_catalog().await?;
        tracing::info!(count = catalog.len(), "loaded the benchmark catalog");
        let catalog_store = stores.catalog.clone();
        let catalog_cache = Arc::new(CatalogCache::new(
            stores.catalog,
            catalog,
            std::time::Duration::from_secs(config.catalog_ttl_secs),
        ));
        let results_db = config
            .duckdb
            .clone()
            .map(|duckdb| ResultsDb::new(http_client.clone(), duckdb))
            .transpose()?;
        let clerk = config
            .clerk
            .as_ref()
            .map(ClerkSignIn::new)
            .transpose()?
            .map(Arc::new);
        Ok(Self {
            config: Arc::new(config),
            catalog_cache,
            catalog_store,
            descriptor_catalog_store: stores.descriptor_catalog,
            catalog_import_runs: stores.catalog_import_runs,
            results_db,
            http_client,
            replay_cache: Arc::new(ReplayCache::new()),
            legacy_user_keys: Arc::new(LegacyUserKeys::new()),
            migrated_clients: Arc::new(MigratedClients::new()),
            liveness_throttle: Arc::new(LivenessThrottle::new()),
            registration_throttle: Arc::new(RegistrationThrottle::new()),
            clerk,
            client_store: stores.clients,
            user_store: stores.users,
            org_store: stores.orgs,
            tag_store: stores.tags,
            key_index_store: stores.key_index,
            submission_store: stores.submissions,
            warehouse_store: stores.warehouse,
            eval_sample_result_store: stores.eval_sample_results,
            todo_store: stores.todo,
            plan_store: stores.plans,
            campaign_store: stores.campaigns,
            model_params_store: stores.model_params,
            audit_store: stores.audit,
            audit_writes: tokio_util::task::TaskTracker::new(),
            dataset_cache_store: stores.dataset_cache,
            vault_store: stores.vault,
            vault_uploads: Arc::new(tokio::sync::Semaphore::new(VAULT_UPLOADS_MAX)),
            submission_bodies: Arc::new(tokio::sync::Semaphore::new(SUBMISSION_SHARES)),
            held_submission_bodies: Arc::new(tokio::sync::Semaphore::new(HELD_SUBMISSION_SHARES)),
            submissions_in_flight: Arc::new(tokio::sync::Semaphore::new(SUBMISSION_SHARES)),
            submission_readers: Arc::default(),
            hf_imports,
            openapi,
            cell_cache: Arc::default(),
            plan_results_cache: Arc::default(),
        })
    }
}
